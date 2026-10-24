//! Platform-independent core: turns local clipboard changes into offers for the
//! peer, and remote offers into clipboard contents (prefetching files into a cache).

use crate::log;
use crate::net::{self, Conn, Reader};
use crate::proto::{Ctl, Entry, Msg};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

const CHUNK: usize = 65000;
const KEEP_OFFERS: usize = 4;
const SKIP_NAMES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];
/// Transfers that finish faster than this never show progress UI.
const SHOW_AFTER: Duration = Duration::from_millis(400);
const SHOW_DONE_FOR: Duration = Duration::from_secs(2);
const SHOW_FAILED_FOR: Duration = Duration::from_secs(6);

/// Clipboard contents read from the local OS.
#[derive(Default, Debug)]
pub struct LocalClip {
    pub text: Option<String>,
    pub html: Option<String>,
    pub png: Option<Vec<u8>>,
    pub files: Vec<PathBuf>,
}

/// Clipboard contents that came from the peer; the platform layer puts it on the OS clipboard.
pub struct RemoteClip {
    pub text: Option<String>,
    pub html: Option<String>,
    pub png: Option<Vec<u8>>,
    pub files: Option<Arc<Transfer>>,
}

#[derive(Clone)]
enum TState {
    Pending,
    Done,
    Failed(String),
    Cancelled,
}

/// A file set moving in either direction. Inbound ones are downloaded into the cache.
pub struct Transfer {
    pub id: u64,
    /// Absolute paths (in the cache) of the top-level entries, in copy order. Empty for uploads.
    pub top: Vec<PathBuf>,
    pub total: u64,
    upload: bool,
    /// "“name”" for a single item, "N items" otherwise.
    label: String,
    started: Instant,
    bytes: AtomicU64,
    state: Mutex<(TState, Option<Instant>)>,
    cv: Condvar,
}

impl Transfer {
    fn new(id: u64, upload: bool, top_names: &[&str], top: Vec<PathBuf>, total: u64) -> Self {
        let label = match top_names {
            [one] => format!("“{one}”"),
            many => format!("{} items", many.len()),
        };
        Transfer {
            id,
            top,
            total,
            upload,
            label,
            started: Instant::now(),
            bytes: AtomicU64::new(0),
            state: Mutex::new((TState::Pending, None)),
            cv: Condvar::new(),
        }
    }

    /// Wait up to `timeout`; `None` if the transfer is still running.
    pub fn wait_step(&self, timeout: Duration) -> Option<Result<(), String>> {
        let g = self.state.lock().unwrap();
        let (g, _) = self
            .cv
            .wait_timeout_while(g, timeout, |s| matches!(s.0, TState::Pending))
            .unwrap();
        match &g.0 {
            TState::Pending => None,
            TState::Done => Some(Ok(())),
            TState::Failed(e) => Some(Err(e.clone())),
            TState::Cancelled => Some(Err("cancelled".into())),
        }
    }

    /// Block until the transfer finishes, fails, or `timeout` elapses.
    #[allow(dead_code)]
    pub fn wait(&self, timeout: Duration) -> Result<(), String> {
        self.wait_step(timeout).unwrap_or_else(|| {
            Err(format!(
                "timed out ({} of {} MB received)",
                self.bytes.load(Ordering::Relaxed) >> 20,
                self.total >> 20
            ))
        })
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state.lock().unwrap().0, TState::Done)
    }

    fn finish(&self, s: TState) {
        let mut g = self.state.lock().unwrap();
        if matches!(g.0, TState::Pending) {
            *g = (s, Some(Instant::now()));
        }
        self.cv.notify_all();
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ViewState {
    Running,
    Done,
    Failed,
}

/// What the platform's progress UI should show right now.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct ProgressView {
    pub id: u64,
    pub state: ViewState,
    /// e.g. "Receiving 3 items from MacBook"
    pub title: String,
    /// e.g. "120 MB of 300 MB · 85 MB/s · 2 s left"
    pub detail: String,
    pub fraction: f64,
}

fn human_bytes(b: u64) -> String {
    let b = b as f64;
    match b {
        b if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        b if b >= 1e6 => format!("{:.0} MB", b / 1e6),
        b if b >= 1e3 => format!("{:.0} KB", b / 1e3),
        b => format!("{b} B"),
    }
}

fn human_secs(s: f64) -> String {
    let s = (s.round() as u64).max(1);
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

struct Offer {
    id: u64,
    files: Vec<(PathBuf, Entry)>,
}

struct Inbound {
    t: Arc<Transfer>,
    root: PathBuf,
    entries: Vec<Entry>,
    cur: Option<(u32, File)>,
}

pub struct Engine {
    name: String,
    peer_name: Mutex<String>,
    /// Most recent file transfer in either direction, for progress UI.
    activity: Mutex<Option<Arc<Transfer>>>,
    conn: Mutex<Option<Arc<Conn>>>,
    offers: Mutex<VecDeque<Offer>>,
    sending: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    inbound: Mutex<Option<Inbound>>,
    to_platform: Sender<RemoteClip>,
    cache: PathBuf,
    max_bytes: u64,
    next_id: AtomicU64,
}

impl Engine {
    pub fn new(name: String, cache: PathBuf, max_bytes: u64, to_platform: Sender<RemoteClip>) -> Arc<Self> {
        let _ = fs::remove_dir_all(&cache);
        let _ = fs::create_dir_all(&cache);
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).unwrap();
        Arc::new(Engine {
            name,
            peer_name: Mutex::new("the other computer".into()),
            activity: Mutex::new(None),
            conn: Mutex::new(None),
            offers: Mutex::new(VecDeque::new()),
            sending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(None),
            to_platform,
            cache,
            max_bytes,
            next_id: AtomicU64::new(u64::from_be_bytes(seed) >> 12),
        })
    }

    fn send(&self, m: &Msg) {
        let c = self.conn.lock().unwrap().clone();
        if let Some(c) = c {
            if let Err(e) = c.send(m) {
                log!("send to {} failed: {e}", c.peer);
                c.close();
            }
        }
    }

    fn connected(&self) -> bool {
        self.conn.lock().unwrap().is_some()
    }

    /// Progress of the latest file transfer, or `None` when nothing should be shown.
    pub fn progress(&self) -> Option<ProgressView> {
        let t = self.activity.lock().unwrap().clone()?;
        let (state, ended) = t.state.lock().unwrap().clone();
        let now = Instant::now();
        let end = ended.unwrap_or(now);
        let elapsed = end - t.started;
        let state = match state {
            TState::Pending if elapsed < SHOW_AFTER => return None,
            TState::Pending => ViewState::Running,
            TState::Done if elapsed < SHOW_AFTER || now - end > SHOW_DONE_FOR => return None,
            TState::Done => ViewState::Done,
            TState::Failed(_) if now - end > SHOW_FAILED_FOR => return None,
            TState::Failed(_) => ViewState::Failed,
            TState::Cancelled => return None,
        };
        let failure = match &t.state.lock().unwrap().0 {
            TState::Failed(e) => e.clone(),
            _ => String::new(),
        };
        let bytes = t.bytes.load(Ordering::Relaxed).min(t.total);
        let fraction = if t.total == 0 {
            1.0
        } else {
            bytes as f64 / t.total as f64
        };
        let peer = self.peer_name.lock().unwrap().clone();
        let (verb, prep) = if t.upload {
            ("Sending", "to")
        } else {
            ("Receiving", "from")
        };
        let secs = elapsed.as_secs_f64().max(0.001);
        let (title, detail) = match state {
            ViewState::Running => {
                let rate = bytes as f64 / secs;
                let mut detail = format!("{} of {}", human_bytes(bytes), human_bytes(t.total));
                if secs > 1.0 && rate > 0.0 {
                    let left = (t.total - bytes) as f64 / rate;
                    detail += &format!(" · {}/s · {} left", human_bytes(rate as u64), human_secs(left));
                }
                (format!("{verb} {} {prep} {peer}", t.label), detail)
            }
            ViewState::Done => (
                format!("{} {}", if t.upload { "Sent" } else { "Received" }, t.label),
                format!("{} in {}", human_bytes(t.total), human_secs(secs)),
            ),
            ViewState::Failed => (format!("Couldn’t copy {}", t.label), failure),
        };
        Some(ProgressView {
            id: t.id,
            state,
            title,
            detail,
            fraction,
        })
    }

    // ---------- local -> remote ----------

    /// Consume local clipboard changes from the platform layer (runs on its own thread).
    pub fn pump_local(self: &Arc<Self>, rx: Receiver<LocalClip>) {
        for clip in rx {
            self.cancel_inbound("superseded by a local copy");
            if !self.connected() {
                continue;
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let mut files = Vec::new();
            for p in &clip.files {
                walk(
                    p,
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    &mut files,
                );
            }
            let total: u64 = files.iter().map(|(_, e)| e.size).sum();
            if !files.is_empty() {
                log!(
                    "offering {} item(s), {} entries, {:.1} MB",
                    clip.files.len(),
                    files.len(),
                    total as f64 / 1e6
                );
            }
            let entries = files.iter().map(|(_, e)| e.clone()).collect();
            {
                let mut o = self.offers.lock().unwrap();
                o.push_back(Offer { id, files });
                while o.len() > KEEP_OFFERS {
                    o.pop_front();
                }
            }
            let png = clip.png.is_some();
            self.send(&Msg::Ctl(
                Ctl::Clip {
                    id,
                    text: clip.text,
                    html: clip.html,
                    png,
                    files: entries,
                },
                clip.png.unwrap_or_default(),
            ));
        }
    }

    fn start_upload(self: &Arc<Self>, id: u64) {
        let files: Vec<(PathBuf, Entry)> = {
            let o = self.offers.lock().unwrap();
            match o.iter().find(|o| o.id == id) {
                Some(o) => o.files.clone(),
                None => {
                    drop(o);
                    self.send(&Msg::Ctl(
                        Ctl::Fail {
                            id,
                            msg: "offer expired".into(),
                        },
                        vec![],
                    ));
                    return;
                }
            }
        };
        let top: Vec<&str> = files
            .iter()
            .filter(|(_, e)| !e.path.contains('/'))
            .map(|(_, e)| e.path.as_str())
            .collect();
        let total = files.iter().map(|(_, e)| e.size).sum();
        let t = Arc::new(Transfer::new(id, true, &top, vec![], total));
        *self.activity.lock().unwrap() = Some(t.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut s = self.sending.lock().unwrap();
            for c in s.values() {
                c.store(true, Ordering::Relaxed); // only one upload at a time
            }
            s.insert(id, cancel.clone());
        }
        let me = self.clone();
        thread::spawn(move || {
            let r = me.upload(&t, &files, &cancel);
            me.sending.lock().unwrap().remove(&id);
            match r {
                Ok(true) => {
                    t.finish(TState::Done);
                    me.send(&Msg::Ctl(Ctl::Done { id }, vec![]));
                }
                Ok(false) if !me.connected() => t.finish(TState::Failed("the other computer disconnected".into())),
                Ok(false) => {
                    t.finish(TState::Cancelled);
                    log!("upload {id} cancelled");
                }
                Err(e) => {
                    t.finish(TState::Failed(e.clone()));
                    log!("upload {id} failed: {e}");
                    me.send(&Msg::Ctl(Ctl::Fail { id, msg: e }, vec![]));
                }
            }
        });
    }

    fn upload(&self, t: &Transfer, files: &[(PathBuf, Entry)], cancel: &AtomicBool) -> Result<bool, String> {
        let id = t.id;
        #[cfg(feature = "devpeer")]
        let throttle = std::env::var("DESKDROP_THROTTLE_MS").ok().and_then(|v| v.parse().ok());
        let conn = self.conn.lock().unwrap().clone().ok_or("not connected")?;
        let mut buf = vec![0u8; CHUNK];
        for (idx, (abs, e)) in files.iter().enumerate() {
            if e.dir {
                continue;
            }
            let mut f = File::open(abs).map_err(|err| format!("{}: {err}", abs.display()))?;
            let mut left = e.size;
            while left > 0 {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                let want = (left as usize).min(CHUNK);
                let n = f.read(&mut buf[..want]).map_err(|err| err.to_string())?;
                if n == 0 {
                    return Err(format!("{} shrank while copying", abs.display()));
                }
                left -= n as u64;
                #[cfg(feature = "devpeer")]
                if let Some(ms) = throttle {
                    thread::sleep(Duration::from_millis(ms));
                }
                conn.send(&Msg::Chunk {
                    id,
                    idx: idx as u32,
                    data: buf[..n].to_vec(),
                })
                .map_err(|e| e.to_string())?;
                t.bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        Ok(true)
    }

    // ---------- remote -> local ----------

    fn cancel_inbound(&self, why: &str) {
        if let Some(ib) = self.inbound.lock().unwrap().take() {
            if !ib.t.is_done() {
                log!("dropping remote copy: {why}");
                ib.t.finish(TState::Cancelled);
                self.send(&Msg::Ctl(Ctl::Cancel { id: ib.t.id }, vec![]));
            }
        }
    }

    fn on_clip(&self, id: u64, text: Option<String>, html: Option<String>, png: Option<Vec<u8>>, entries: Vec<Entry>) {
        self.cancel_inbound("superseded by a newer remote copy");
        let mut files = None;
        let total: u64 = entries.iter().map(|e| e.size).sum();
        if !entries.is_empty() {
            if total > self.max_bytes {
                log!(
                    "remote copy is {:.1} GB, over the {:.1} GB limit; files not offered",
                    total as f64 / 1e9,
                    self.max_bytes as f64 / 1e9
                );
            } else {
                match self.prepare(id, &entries, total) {
                    Ok(ib) => {
                        files = Some(ib.t.clone());
                        *self.activity.lock().unwrap() = Some(ib.t.clone());
                        *self.inbound.lock().unwrap() = Some(ib);
                        self.send(&Msg::Ctl(Ctl::Fetch { id }, vec![]));
                        log!("receiving {} entries, {:.1} MB", entries.len(), total as f64 / 1e6);
                    }
                    Err(e) => log!("cannot prepare cache: {e}"),
                }
            }
        }
        let _ = self.to_platform.send(RemoteClip { text, html, png, files });
    }

    fn prepare(&self, id: u64, entries: &[Entry], total: u64) -> std::io::Result<Inbound> {
        self.prune_cache();
        let root = self.cache.join(format!("{id:x}"));
        fs::create_dir_all(&root)?;
        let mut top = Vec::new();
        let mut clean = Vec::with_capacity(entries.len());
        for e in entries {
            let rel = sanitize(&e.path).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad path {:?}", e.path))
            })?;
            let p = root.join(&rel);
            if e.dir {
                fs::create_dir_all(&p)?;
            } else {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent)?;
                }
                File::create(&p)?;
            }
            if !e.path.contains('/') {
                top.push(p);
            }
            clean.push(Entry {
                path: rel.to_string_lossy().into_owned(),
                ..e.clone()
            });
        }
        let done = total == 0;
        let names: Vec<&str> = entries
            .iter()
            .filter(|e| !e.path.contains('/'))
            .map(|e| e.path.as_str())
            .collect();
        let t = Arc::new(Transfer::new(id, false, &names, top, total));
        let ib = Inbound {
            t,
            root,
            entries: clean,
            cur: None,
        };
        if done {
            finalize(&ib);
            ib.t.finish(TState::Done);
        }
        Ok(ib)
    }

    /// Keep only the most recent couple of cache dirs (a paste may still be reading the previous one).
    fn prune_cache(&self) {
        let Ok(rd) = fs::read_dir(&self.cache) else {
            return;
        };
        let mut dirs: Vec<_> = rd
            .flatten()
            .filter_map(|d| Some((d.metadata().ok()?.modified().ok()?, d.path())))
            .collect();
        dirs.sort();
        let n = dirs.len();
        for (_, p) in dirs.into_iter().take(n.saturating_sub(2)) {
            let _ = fs::remove_dir_all(p);
        }
    }

    fn on_chunk(&self, id: u64, idx: u32, data: &[u8]) {
        let mut g = self.inbound.lock().unwrap();
        let Some(ib) = g.as_mut().filter(|ib| ib.t.id == id) else {
            return;
        };
        if ib.cur.as_ref().map(|c| c.0) != Some(idx) {
            let Some(e) = ib.entries.get(idx as usize) else {
                return;
            };
            match OpenOptions::new().write(true).open(ib.root.join(&e.path)) {
                Ok(f) => ib.cur = Some((idx, f)),
                Err(err) => {
                    let msg = format!("open {}: {err}", e.path);
                    ib.t.finish(TState::Failed(msg.clone()));
                    drop(g);
                    log!("{msg}");
                    self.send(&Msg::Ctl(Ctl::Cancel { id }, vec![]));
                    return;
                }
            }
        }
        if let Err(err) = ib.cur.as_mut().unwrap().1.write_all(data) {
            let msg = format!("write: {err}");
            ib.t.finish(TState::Failed(msg.clone()));
            drop(g);
            log!("{msg}");
            self.send(&Msg::Ctl(Ctl::Cancel { id }, vec![]));
            return;
        }
        ib.t.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
    }

    fn on_done(&self, id: u64, err: Option<String>) {
        let mut g = self.inbound.lock().unwrap();
        let Some(ib) = g.as_mut().filter(|ib| ib.t.id == id) else {
            return;
        };
        ib.cur = None;
        match err {
            None => {
                finalize(ib);
                log!("received {:.1} MB", ib.t.total as f64 / 1e6);
                ib.t.finish(TState::Done);
            }
            Some(e) => {
                log!("peer failed to send files: {e}");
                ib.t.finish(TState::Failed(e));
            }
        }
    }

    fn handle(self: &Arc<Self>, m: Msg) {
        match m {
            Msg::Chunk { id, idx, data } => self.on_chunk(id, idx, &data),
            Msg::Ctl(c, blob) => match c {
                Ctl::Hello { name, version } => {
                    log!("peer is {name} (deskdrop {version})");
                    *self.peer_name.lock().unwrap() = name;
                }
                Ctl::Clip {
                    id,
                    text,
                    html,
                    png,
                    files,
                } => self.on_clip(id, text, html, png.then_some(blob), files),
                Ctl::Fetch { id } => self.start_upload(id),
                Ctl::Cancel { id } => {
                    if let Some(c) = self.sending.lock().unwrap().get(&id) {
                        c.store(true, Ordering::Relaxed);
                    }
                }
                Ctl::Done { id } => self.on_done(id, None),
                Ctl::Fail { id, msg } => self.on_done(id, Some(msg)),
                Ctl::Ping => {}
            },
        }
    }

    // ---------- connection management ----------

    fn session(self: &Arc<Self>, conn: Arc<Conn>, mut reader: Reader) {
        log!("connected to {}", conn.peer);
        if let Some(old) = self.conn.lock().unwrap().replace(conn.clone()) {
            old.close();
        }
        self.send(&Msg::Ctl(
            Ctl::Hello {
                name: self.name.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            vec![],
        ));
        let err = loop {
            match reader.recv() {
                Ok(m) => self.handle(m),
                Err(e) => break e,
            }
        };
        conn.close();
        let mut g = self.conn.lock().unwrap();
        if g.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
            *g = None;
            drop(g);
            log!("disconnected from {}: {err}", conn.peer);
            for c in self.sending.lock().unwrap().values() {
                c.store(true, Ordering::Relaxed);
            }
            if let Some(ib) = self.inbound.lock().unwrap().as_ref() {
                ib.t.finish(TState::Failed("peer disconnected".into()));
            }
        }
    }

    pub fn listen(self: Arc<Self>, addr: String, psk: [u8; 32]) {
        let l = match TcpListener::bind(&addr) {
            Ok(l) => l,
            Err(e) => {
                log!("cannot listen on {addr}: {e}");
                std::process::exit(1);
            }
        };
        log!("listening on {addr}");
        for s in l.incoming().flatten() {
            let me = self.clone();
            thread::spawn(move || match net::handshake(s, &psk, false) {
                Ok((c, r)) => me.session(c, r),
                Err(e) => log!("rejected connection: {e}"),
            });
        }
    }

    pub fn connect(self: Arc<Self>, addrs: Vec<String>, psk: [u8; 32]) {
        let mut last_err = String::new();
        loop {
            for a in &addrs {
                match dial(a).and_then(|s| net::handshake(s, &psk, true)) {
                    Ok((c, r)) => {
                        last_err.clear();
                        self.session(c, r);
                    }
                    Err(e) => {
                        let msg = format!("{a}: {e}");
                        if msg != last_err {
                            log!("connect failed, retrying: {msg}");
                            last_err = msg;
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    pub fn keepalive(self: Arc<Self>) {
        loop {
            thread::sleep(Duration::from_secs(15));
            self.send(&Msg::Ctl(Ctl::Ping, vec![]));
        }
    }
}

fn dial(a: &str) -> std::io::Result<TcpStream> {
    let mut err = std::io::Error::new(std::io::ErrorKind::NotFound, "no address");
    for sa in a.to_socket_addrs()? {
        match TcpStream::connect_timeout(&sa, Duration::from_secs(3)) {
            Ok(s) => return Ok(s),
            Err(e) => err = e,
        }
    }
    Err(err)
}

fn mtime_of(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

fn walk(abs: &Path, rel: String, out: &mut Vec<(PathBuf, Entry)>) {
    if SKIP_NAMES.iter().any(|n| rel.ends_with(n)) {
        return;
    }
    let Ok(md) = fs::metadata(abs) else { return };
    if md.is_dir() {
        out.push((
            abs.to_path_buf(),
            Entry {
                path: rel.clone(),
                size: 0,
                dir: true,
                mtime: mtime_of(&md),
            },
        ));
        let Ok(rd) = fs::read_dir(abs) else { return };
        let mut kids: Vec<_> = rd.flatten().collect();
        kids.sort_by_key(|d| d.file_name());
        for d in kids {
            // Don't follow symlinked dirs (loops); symlinked files are copied as files.
            if d.file_type().is_ok_and(|t| t.is_symlink()) && d.path().is_dir() {
                continue;
            }
            let name = d.file_name().to_string_lossy().into_owned();
            walk(&d.path(), format!("{rel}/{name}"), out);
        }
    } else if md.is_file() {
        out.push((
            abs.to_path_buf(),
            Entry {
                path: rel,
                size: md.len(),
                dir: false,
                mtime: mtime_of(&md),
            },
        ));
    }
}

/// Map a peer-supplied relative path to a safe local relative path.
fn sanitize(rel: &str) -> Option<PathBuf> {
    let mut p = PathBuf::new();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return None;
        }
        #[cfg(windows)]
        let part = {
            let mut s: String = part
                .chars()
                .map(|c| {
                    if "<>:\"|?*".contains(c) || (c as u32) < 32 {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            while s.ends_with('.') || s.ends_with(' ') {
                s.pop();
            }
            if s.is_empty() {
                s.push('_');
            }
            s
        };
        p.push(part);
    }
    Some(p)
}

fn finalize(ib: &Inbound) {
    // Children first so directory mtimes aren't bumped by later writes.
    for e in ib.entries.iter().rev() {
        let t = filetime::FileTime::from_unix_time(e.mtime, 0);
        let _ = filetime::set_file_mtime(ib.root.join(&e.path), t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_view_lifecycle() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let dir = std::env::temp_dir().join(format!("deskdrop-test-{}", std::process::id()));
        let eng = Engine::new("me".into(), dir.clone(), u64::MAX, tx);
        *eng.peer_name.lock().unwrap() = "PC".into();
        let mut t = Transfer::new(7, false, &["a", "b"], vec![], 300_000_000);
        t.started = Instant::now() - Duration::from_secs(2);
        let t = Arc::new(t);
        t.bytes.store(100_000_000, Ordering::Relaxed);
        *eng.activity.lock().unwrap() = Some(t.clone());
        let v = eng.progress().unwrap();
        assert_eq!(v.state, ViewState::Running);
        assert_eq!(v.title, "Receiving 2 items from PC");
        assert!((v.fraction - 1.0 / 3.0).abs() < 1e-9);
        assert!(
            v.detail.starts_with("100 MB of 300 MB · 50 MB/s · 4 s left"),
            "{}",
            v.detail
        );
        t.finish(TState::Done);
        assert_eq!(eng.progress().unwrap().state, ViewState::Done);

        // Fast transfers never show anything.
        let quick = Arc::new(Transfer::new(8, true, &["x.bin"], vec![], 10));
        quick.finish(TState::Done);
        *eng.activity.lock().unwrap() = Some(quick);
        assert!(eng.progress().is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
