// Auto-Edit desktop — Tauri v2 shell.
// The heavy lifting lives in the Python engine (`auto-edit serve`); this window
// just loads the web UI in ../ui, which talks to the local API over HTTP + SSE.
//
// On launch the shell starts the engine itself unless one is already listening
// on ENGINE_ADDR (e.g. you ran `auto-edit serve` by hand — that one is reused
// and left alone). The engine we started is killed when the app exits.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, File};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{Manager, RunEvent};

const ENGINE_ADDR: &str = "127.0.0.1:8760";

/// The engine process this app spawned (None if we reused an existing one).
struct Engine(Mutex<Option<Child>>);

fn engine_listening() -> bool {
    let addr: SocketAddr = ENGINE_ADDR.parse().unwrap();
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Repo checkout this binary was built from (desktop/src-tauri/../..). Only
/// meaningful for dev builds running from the source tree.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Find the `auto-edit` executable. A GUI app launched from Finder/Dock does
/// not inherit the shell's PATH, so look in the usual install spots before
/// asking the user's login shell.
fn find_auto_edit() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AUTO_EDIT_BIN") {
        return Some(PathBuf::from(p));
    }

    let exe = if cfg!(windows) { "auto-edit.exe" } else { "auto-edit" };
    let venv_bin = if cfg!(windows) { "Scripts" } else { "bin" };
    let mut candidates = vec![repo_root().join(".venv").join(venv_bin).join(exe)];
    if let Some(h) = home() {
        candidates.push(h.join(".local/bin").join(exe)); // uv tool / pipx
        candidates.push(h.join(".nix-profile/bin").join(exe));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(exe));
    candidates.push(PathBuf::from("/usr/local/bin").join(exe));
    if let Some(p) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(p);
    }

    lookup_in_shell()
}

#[cfg(unix)]
fn lookup_in_shell() -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = Command::new(shell).args(["-lc", "command -v auto-edit"]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).lines().last()?.trim().to_string();
    let p = PathBuf::from(line);
    p.is_file().then_some(p)
}

#[cfg(windows)]
fn lookup_in_shell() -> Option<PathBuf> {
    let out = Command::new("where").arg("auto-edit").output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    let p = PathBuf::from(line);
    p.is_file().then_some(p)
}

/// Where the engine runs. `serve` keeps its library in `<cwd>/workspace`
/// (unless AUTO_EDIT_WORKSPACE says otherwise): the repo in dev, so you see the
/// same workspaces as the CLI; ~/.auto-edit in a packaged app.
fn engine_cwd() -> PathBuf {
    if let Some(p) = std::env::var_os("AUTO_EDIT_CWD") {
        return PathBuf::from(p);
    }
    if cfg!(debug_assertions) {
        return repo_root();
    }
    let dir = std::env::var_os("AUTO_EDIT_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".auto-edit")))
        .unwrap_or_else(std::env::temp_dir);
    let _ = fs::create_dir_all(&dir);
    dir
}

fn start_engine(log_dir: &Path) -> Result<Child, String> {
    let bin = find_auto_edit().ok_or(
        "`auto-edit` não encontrado. Instale o CLI ou aponte AUTO_EDIT_BIN pro executável.",
    )?;

    let _ = fs::create_dir_all(log_dir);
    let log_path = log_dir.join("engine.log");
    let log = File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;

    let mut cmd = Command::new(&bin);
    cmd.arg("serve")
        .current_dir(engine_cwd())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd.spawn().map_err(|e| format!("{}: {e}", bin.display()))?;
    eprintln!("[auto-edit] engine started: {} serve (log: {})", bin.display(), log_path.display());
    Ok(child)
}

fn main() {
    let app = tauri::Builder::default()
        .manage(Engine(Mutex::new(None)))
        .setup(|app| {
            if engine_listening() {
                eprintln!("[auto-edit] engine already running on {ENGINE_ADDR}, reusing it");
                return Ok(());
            }
            let log_dir = app
                .path()
                .app_log_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("auto-edit"));
            match start_engine(&log_dir) {
                Ok(child) => *app.state::<Engine>().0.lock().unwrap() = Some(child),
                // Not fatal: the UI shows the offline banner and keeps polling.
                Err(e) => eprintln!("[auto-edit] could not start engine: {e}"),
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while running Auto-Edit");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            if let Some(mut child) = handle.state::<Engine>().0.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    });
}
