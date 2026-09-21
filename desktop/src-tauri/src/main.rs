#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The server always listens on a stable port so the webview origin
/// (`http://127.0.0.1:<port>`) never changes between launches. WebView2 stores
/// localStorage / IndexedDB per origin, so a random port would silently discard
/// the user's settings and courses on every start.
const DEFAULT_PORT: u16 = 39217;
const PORT_FALLBACKS: u16 = 5;

/// Holds the spawned Node server so it can be killed when the app exits.
struct ServerState(Mutex<Option<Child>>);

#[derive(serde::Deserialize, serde::Serialize)]
struct DesktopConfig {
    /// Preferred server port. Change this only if 39217 is taken by something else.
    port: u16,
    /// Extra environment variables handed to the Node server process.
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            env: BTreeMap::new(),
        }
    }
}

fn log_path() -> PathBuf {
    std::env::temp_dir().join("openmaic-desktop.log")
}

fn log(msg: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// `%LOCALAPPDATA%\OpenMAIC-desktop\desktop-config.json`
///
/// Deliberately outside both the install directory (replaced on update) and the
/// WebView2 data directory (`%LOCALAPPDATA%\chat.maic.openmaic`, which the
/// uninstaller can remove), so the file survives updates and re-installs.
fn config_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base)
        .join("OpenMAIC-desktop")
        .join("desktop-config.json")
}

fn load_config() -> DesktopConfig {
    let path = config_path();
    if let Ok(raw) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<DesktopConfig>(&raw) {
            Ok(cfg) => {
                log(&format!("[config] loaded {}", path.display()));
                return cfg;
            }
            Err(err) => {
                log(&format!("[config] invalid {}: {err}; using defaults", path.display()));
            }
        }
    }

    let cfg = DesktopConfig::default();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&cfg) {
        if std::fs::write(&path, text).is_ok() {
            log(&format!("[config] wrote default {}", path.display()));
        }
    }
    cfg
}

/// Tauri returns resource paths with the `\\?\` extended-length prefix on
/// Windows. Node cannot parse that prefix (it reads `\\?\D:\...` as `D:`),
/// so strip it before handing paths to the Node process.
fn clean_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn pick_ephemeral_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(DEFAULT_PORT)
}

fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Best-effort probe: does something already answer as an OpenMAIC server?
fn is_openmaic_server(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let request = format!(
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body);
    body.contains(" 200") && body.contains("\"status\"")
}

enum PortChoice {
    /// Port is free — start our own server there.
    Start(u16),
    /// A previous instance still owns the port — reuse it so the origin is kept.
    Reuse(u16),
}

fn choose_port(preferred: u16) -> PortChoice {
    let mut candidates = vec![preferred];
    for offset in 1..=PORT_FALLBACKS {
        candidates.push(preferred.wrapping_add(offset));
    }
    for port in candidates {
        if port_is_free(port) {
            return PortChoice::Start(port);
        }
        if is_openmaic_server(port) {
            return PortChoice::Reuse(port);
        }
        log(&format!("[port] {port} busy (not OpenMAIC), trying next"));
    }
    log("[port] no candidate free, falling back to an ephemeral port");
    PortChoice::Start(pick_ephemeral_port())
}

fn spawn_server(resource_dir: &Path, port: u16, extra_env: &BTreeMap<String, String>) -> std::io::Result<Child> {
    let node = clean_path(&resource_dir.join("node").join("node.exe"));
    let server = clean_path(&resource_dir.join("app").join("server.js"));
    let workdir = clean_path(&resource_dir.join("app"));

    log(&format!("[setup] resource_dir = {}", resource_dir.display()));
    log(&format!("[setup] node         = {}", node.display()));
    log(&format!("[setup] server       = {}", server.display()));
    log(&format!("[setup] node exists  = {}", node.exists()));
    log(&format!("[setup] server exists= {}", server.exists()));

    let mut cmd = Command::new(&node);
    cmd.arg(&server)
        .current_dir(&workdir)
        .env("PORT", port.to_string())
        .env("HOSTNAME", "127.0.0.1")
        .env("NODE_ENV", "production")
        .env("NEXT_TELEMETRY_DISABLED", "1");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let tmp = std::env::temp_dir();
    match File::create(tmp.join("openmaic-node.out.log")) {
        Ok(f) => {
            cmd.stdout(Stdio::from(f));
        }
        Err(_) => {
            cmd.stdout(Stdio::null());
        }
    }
    match File::create(tmp.join("openmaic-node.err.log")) {
        Ok(f) => {
            cmd.stderr(Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    cmd.spawn()
}

fn main() {
    let _ = std::fs::remove_file(log_path());
    log("=== openmaic-desktop starting ===");

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let config = load_config();
            log(&format!("[config] preferred port = {}", config.port));

            let choice = choose_port(config.port);
            let port = match &choice {
                PortChoice::Start(p) => *p,
                PortChoice::Reuse(p) => *p,
            };

            let resource_dir = app.path().resource_dir()?;

            match choice {
                PortChoice::Start(port) => {
                    match spawn_server(&resource_dir, port, &config.env) {
                        Ok(child) => {
                            log(&format!("[setup] node spawned, pid = {}", child.id()));
                            app.manage(ServerState(Mutex::new(Some(child))));
                        }
                        Err(err) => {
                            log(&format!("[setup] spawn failed: {err}"));
                        }
                    }
                }
                PortChoice::Reuse(port) => {
                    log(&format!("[setup] reusing server already listening on {port}"));
                }
            }

            // Wait for the server to accept connections, then point the window at it.
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(120);
                let mut ready = false;
                while Instant::now() < deadline {
                    if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                        ready = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(400));
                }
                log(&format!("[setup] server ready = {ready} on port {port}"));

                if let Some(window) = handle.get_webview_window("main") {
                    if ready {
                        let url = format!("http://127.0.0.1:{port}");
                        let _ = window.eval(&format!("window.location.replace('{url}')"));
                    } else {
                        let _ = window.eval(
                            "document.body.innerHTML = '<div style=\"font-family:Segoe UI;padding:40px;text-align:center\"><h2>OpenMAIC 启动失败</h2><p>本地服务未能在 120 秒内就绪，请尝试重新安装。</p></div>';",
                        );
                    }
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                log("[exit] killing node server");
                if let Some(state) = app_handle.try_state::<ServerState>() {
                    if let Ok(mut guard) = state.0.lock() {
                        if let Some(mut child) = guard.take() {
                            let _ = child.kill();
                        }
                    }
                }
            }
        });
}
