//! Debug-only headless peer for testing one real agent without a second OS.
//! `deskdrop devpeer HOST:PORT SECRET`, then stdin lines: `text <s>` or `files <path>...`.

use crate::engine::{Engine, LocalClip};
use crate::{log, net};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

pub fn run(args: &[String]) -> Result<(), String> {
    let [addr, secret] = args else {
        return Err("usage: devpeer HOST:PORT SECRET".into());
    };
    let psk = net::psk_from_secret(secret);
    let (local_tx, local_rx) = channel();
    let (remote_tx, remote_rx) = channel();
    let cache = std::env::temp_dir().join(format!("deskdrop-devpeer-{}", std::process::id()));
    let eng = Engine::new("devpeer".into(), cache, u64::MAX, remote_tx);
    let e = eng.clone();
    thread::spawn(move || e.pump_local(local_rx));
    let e = eng.clone();
    thread::spawn(move || e.keepalive());
    let a = addr.clone();
    thread::spawn(move || eng.connect(vec![a], psk));
    thread::spawn(move || {
        for c in remote_rx {
            log!(
                "REMOTE text={:?} html={} png={}",
                c.text,
                c.html.is_some(),
                c.png.as_ref().map_or(0, |p| p.len())
            );
            if let Some(t) = c.files {
                let r = t.wait(Duration::from_secs(120));
                log!("REMOTE files {:?} -> {:?}", t.top, r);
            }
        }
    });
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let (cmd, rest) = line.split_once(' ').unwrap_or((&line, ""));
        let clip = match cmd {
            "text" => LocalClip {
                text: Some(rest.into()),
                ..Default::default()
            },
            "files" => LocalClip {
                files: rest.split_whitespace().map(PathBuf::from).collect(),
                ..Default::default()
            },
            _ => continue,
        };
        let _ = local_tx.send(clip);
    }
    thread::sleep(Duration::from_secs(1));
    Ok(())
}
