#![cfg_attr(windows, windows_subsystem = "windows")]

mod cfhtml;
#[cfg(feature = "devpeer")]
mod devpeer;
mod engine;
mod img;
mod net;
mod platform;
mod proto;

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::channel;
use std::sync::{Mutex, OnceLock};
use std::thread;

pub const DEFAULT_PORT: u16 = 24802;

static LOG_FILE: OnceLock<Mutex<fs::File>> = OnceLock::new();

pub fn log_line(s: &str) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (h, m, sec) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let line = format!("[{h:02}:{m:02}:{sec:02}Z] {s}\n");
    eprint!("{line}");
    if let Some(f) = LOG_FILE.get() {
        let _ = f.lock().unwrap().write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log_line(&format!($($t)*)) };
}

#[derive(Serialize, Deserialize)]
struct Config {
    secret: String,
    /// Accept the peer on this address (e.g. "0.0.0.0:24802").
    #[serde(default)]
    listen: Option<String>,
    /// Or dial the peer, trying each address in turn.
    #[serde(default)]
    connect: Vec<String>,
    /// Remote copies larger than this are not offered (protects disk space).
    #[serde(default = "default_max_gb")]
    max_transfer_gb: f64,
}

fn default_max_gb() -> f64 {
    16.0
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default())
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(home)
}

/// Where config, log and the installed binary live.
fn app_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DESKDROP_HOME") {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        env_dir("LOCALAPPDATA").join("deskdrop")
    } else {
        home().join("Library/Application Support/deskdrop")
    }
}

fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DESKDROP_HOME") {
        return PathBuf::from(d).join("cache");
    }
    if cfg!(windows) {
        env_dir("LOCALAPPDATA").join("deskdrop").join("cache")
    } else {
        home().join("Library/Caches/deskdrop")
    }
}

fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

fn load_config() -> Result<Config, String> {
    let p = config_path();
    let s = fs::read_to_string(&p).map_err(|e| {
        format!(
            "{}: {e} (run `deskdrop init-server` or `init-client` first)",
            p.display()
        )
    })?;
    serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
}

fn save_config(c: &Config) -> Result<(), String> {
    fs::create_dir_all(app_dir()).map_err(|e| e.to_string())?;
    fs::write(config_path(), serde_json::to_string_pretty(c).unwrap()).map_err(|e| e.to_string())
}

fn hostname() -> String {
    if cfg!(windows) {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".into())
    } else {
        Command::new("scutil")
            .args(["--get", "LocalHostName"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "mac".into())
    }
}

fn lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

fn random_secret() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn run() -> Result<(), String> {
    let cfg = load_config()?;
    let _ = fs::create_dir_all(app_dir());
    if let Ok(f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(app_dir().join("deskdrop.log"))
    {
        let _ = LOG_FILE.set(Mutex::new(f));
    }
    log!("deskdrop {} starting on {}", env!("CARGO_PKG_VERSION"), hostname());
    let psk = net::psk_from_secret(&cfg.secret);
    let (local_tx, local_rx) = channel();
    let (remote_tx, remote_rx) = channel();
    let max = (cfg.max_transfer_gb * 1e9) as u64;
    let eng = engine::Engine::new(hostname(), cache_dir(), max, remote_tx);
    {
        let e = eng.clone();
        thread::spawn(move || e.pump_local(local_rx));
    }
    {
        let e = eng.clone();
        thread::spawn(move || e.keepalive());
    }
    match (cfg.listen, cfg.connect) {
        (Some(addr), _) => {
            thread::spawn(move || eng.listen(addr, psk));
        }
        (None, addrs) if !addrs.is_empty() => {
            thread::spawn(move || eng.connect(addrs, psk));
        }
        _ => return Err("config needs either `listen` or `connect`".into()),
    }
    platform::run(local_tx, remote_rx)
}

/// Split off a trailing `--install` flag.
fn wants_install(args: &[String]) -> (Vec<String>, bool) {
    let rest: Vec<String> = args.iter().filter(|a| *a != "--install").cloned().collect();
    let inst = rest.len() != args.len();
    (rest, inst)
}

fn init_server(args: &[String]) -> Result<(), String> {
    let (args, inst) = wants_install(args);
    let port = args.first().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    let secret = load_config().map(|c| c.secret).unwrap_or_else(|_| random_secret());
    save_config(&Config {
        secret: secret.clone(),
        listen: Some(format!("0.0.0.0:{port}")),
        connect: vec![],
        max_transfer_gb: 16.0,
    })?;
    let mut addrs = vec![format!("{}.local:{port}", hostname())];
    if let Some(ip) = lan_ip() {
        addrs.insert(0, format!("{ip}:{port}"));
    }
    println!("Wrote {}", config_path().display());
    println!(
        "\nOn the other computer run:\n\n  deskdrop init-client {} {secret} --install\n",
        addrs.join(",")
    );
    if inst {
        install()
    } else {
        Ok(())
    }
}

fn init_client(args: &[String]) -> Result<(), String> {
    let (args, inst) = wants_install(args);
    let [addrs, secret] = &args[..] else {
        return Err("usage: deskdrop init-client HOST:PORT[,HOST:PORT...] SECRET".into());
    };
    let connect = addrs
        .split(',')
        .map(|a| {
            if a.contains(':') {
                a.to_string()
            } else {
                format!("{a}:{DEFAULT_PORT}")
            }
        })
        .collect();
    save_config(&Config {
        secret: secret.clone(),
        listen: None,
        connect,
        max_transfer_gb: 16.0,
    })?;
    println!("Wrote {}", config_path().display());
    if inst {
        install()
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
const LABEL: &str = "com.deskdrop.agent";

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn install() -> Result<(), String> {
    load_config()?;
    let bin = app_dir().join("deskdrop");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if me != bin {
        let _ = fs::remove_file(&bin); // replace atomically-ish; a running copy keeps its inode
        fs::copy(&me, &bin).map_err(|e| format!("copy binary: {e}"))?;
    }
    let log = app_dir().join("launchd.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        bin.display(),
        log.display()
    );
    fs::create_dir_all(plist_path().parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(plist_path(), plist).map_err(|e| e.to_string())?;
    let domain = format!("gui/{}", uid());
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .output();
    let out = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(plist_path())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("launchctl bootstrap: {}", String::from_utf8_lossy(&out.stderr)));
    }
    println!(
        "Installed {} and started it (starts at login).\nLog: {}",
        bin.display(),
        app_dir().join("deskdrop.log").display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<(), String> {
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
        .output();
    let _ = fs::remove_file(plist_path());
    println!("Stopped and removed the login agent.");
    Ok(())
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn install() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    load_config()?;
    let bin = app_dir().join("deskdrop.exe");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = Command::new("taskkill")
        .args([
            "/F",
            "/IM",
            "deskdrop.exe",
            "/FI",
            &format!("PID ne {}", std::process::id()),
        ])
        .output();
    if me != bin {
        thread::sleep(std::time::Duration::from_millis(500));
        fs::copy(&me, &bin).map_err(|e| format!("copy binary: {e}"))?;
    }
    let val = format!("\"{}\" run", bin.display());
    let out = Command::new("reg")
        .args(["add", RUN_KEY, "/v", "deskdrop", "/t", "REG_SZ", "/d", &val, "/f"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("reg add: {}", String::from_utf8_lossy(&out.stderr)));
    }
    const DETACHED_PROCESS: u32 = 0x8;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(&bin)
        .arg("run")
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    println!(
        "Installed {} and started it (starts at login).\nLog: {}",
        bin.display(),
        app_dir().join("deskdrop.log").display()
    );
    Ok(())
}

#[cfg(windows)]
fn uninstall() -> Result<(), String> {
    let _ = Command::new("reg")
        .args(["delete", RUN_KEY, "/v", "deskdrop", "/f"])
        .output();
    let _ = Command::new("taskkill")
        .args([
            "/F",
            "/IM",
            "deskdrop.exe",
            "/FI",
            &format!("PID ne {}", std::process::id()),
        ])
        .output();
    println!("Stopped deskdrop and removed it from startup.");
    Ok(())
}

const USAGE: &str = "deskdrop: copy/paste text, images and files between macOS and Windows

  deskdrop init-server [PORT] [--install]            accept the peer here (prints the client command)
  deskdrop init-client HOST:PORT SECRET [--install]  connect to the server (comma-separate fallbacks)
  deskdrop install                       run at login and start now
  deskdrop uninstall                     stop and remove from login
  deskdrop run                           run in the foreground
";

fn main() {
    #[cfg(windows)]
    unsafe {
        // GUI-subsystem binary: borrow the console we were launched from, if any.
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("run") => run(),
        Some("init-server") => init_server(&args[1..]),
        Some("init-client") => init_client(&args[1..]),
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        #[cfg(feature = "devpeer")]
        Some("devpeer") => devpeer::run(&args[1..]),
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    };
    if let Err(e) = r {
        log!("error: {e}");
        std::process::exit(1);
    }
}
