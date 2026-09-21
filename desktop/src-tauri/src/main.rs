#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Holds the spawned Node server so it can be killed when the app exits.
struct ServerState(Mutex<Option<Child>>);

fn log_path() -> std::path::PathBuf {
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

/// Tauri returns resource paths with the `\\?\` extended-length prefix on
/// Windows. Node cannot parse that prefix (it reads `\\?\D:\...` as `D:`),
/// so strip it before handing paths to the Node process.
fn clean_path(path: &Path) -> std::path::PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        std::path::PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn pick_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(3311)
}

fn spawn_server(resource_dir: &Path, port: u16) -> std::io::Result<Child> {
    let node = clean_path(&resource_dir.join("node").join("node.exe"));
    let server = clean_path(&resource_dir.join("app").join("server.js"));
    let workdir = clean_path(&resource_dir.join("app"));

    log(&format!("[setup] resource_dir = {}", resource_dir.display()));
    log(&format!("[setup] node         = {}", node.display()));
    log(&format!("[setup] server       = {}", server.display()));
    log(&format!("[setup] workdir      = {}", workdir.display()));
    log(&format!("[setup] node exists  = {}", node.exists()));
    log(&format!("[setup] server exists= {}", server.exists()));

    let mut cmd = Command::new(&node);
    cmd.arg(&server)
        .current_dir(&workdir)
        .env("PORT", port.to_string())
        .env("HOSTNAME", "127.0.0.1")
        .env("NODE_ENV", "production")
        .env("NEXT_TELEMETRY_DISABLED", "1");

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
            let port = pick_free_port();
            let resource_dir = app.path().resource_dir()?;

            match spawn_server(&resource_dir, port) {
                Ok(child) => {
                    log(&format!("[setup] node spawned, pid = {}", child.id()));
                    app.manage(ServerState(Mutex::new(Some(child))));
                }
                Err(err) => {
                    log(&format!("[setup] spawn failed: {err}"));
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
