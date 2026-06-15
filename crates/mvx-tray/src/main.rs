//! StatusNotifierItem tray helper for MVX. Intended for Noctalia/Waybar/SNI hosts.
//!
//! The tray is deliberately a separate process: the audio daemon must survive
//! TUI/tray restarts.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{ToolTip, Tray};

use mvx_core::{socket_path, Snapshot};
use mvx_ipc::{Client, Request};

fn main() -> Result<()> {
    // Only one SNI tray may exist at a time. The lock is released by the
    // kernel when the process exits, so a crash never leaves a stale PID.
    let Some(_instance) = acquire_instance_lock()? else {
        return Ok(());
    };

    let tray = MvxTray::new();
    let handle = tray
        .spawn()
        .context("no se pudo registrar el StatusNotifierItem")?;

    let refresh_handle = handle.clone();
    std::thread::Builder::new()
        .name("mvx-tray-refresh".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            let state = fetch_state();
            let updated = refresh_handle.update(|tray: &mut MvxTray| {
                tray.apply(state);
            });
            if updated.is_none() {
                break;
            }
        })?;

    // Keep the process alive; ksni owns its DBus worker thread.
    loop {
        std::thread::park();
    }
}

struct InstanceLock {
    path: PathBuf,
    _file: File,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_instance_lock() -> Result<Option<InstanceLock>> {
    let dir =
        mvx_core::ensure_runtime_dir().context("no se pudo crear el directorio de ejecución")?;
    let path = dir.join("mvx-tray.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("no se pudo abrir {}", path.display()))?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(err).with_context(|| format!("no se pudo bloquear {}", path.display()));
    }

    file.set_len(0)?;
    let mut handle = &file;
    writeln!(handle, "{}", std::process::id())?;
    Ok(Some(InstanceLock { path, _file: file }))
}

struct MvxTray {
    state: Snapshot,
    error: Option<String>,
}

impl MvxTray {
    fn new() -> Self {
        Self {
            state: fetch_state().unwrap_or_else(|err| {
                let mut snap = Snapshot::initial();
                snap.error = Some(err.to_string());
                snap
            }),
            error: None,
        }
    }

    fn apply(&mut self, state: Result<Snapshot>) {
        match state {
            Ok(snap) => {
                self.state = snap;
                self.error = None;
            }
            Err(err) => {
                self.error = Some(err.to_string());
            }
        }
    }

    fn status_text(&self) -> String {
        if let Some(err) = &self.error {
            return format!("mvxd no disponible: {err}");
        }
        if self.state.error.is_some() {
            return "MVX con errores".into();
        }
        if self.state.running {
            "MVX activo".into()
        } else {
            "MVX detenido".into()
        }
    }

    fn input_text(&self) -> String {
        self.state
            .selected_input_description
            .clone()
            .or_else(|| self.state.selected_input.clone())
            .unwrap_or_else(|| "sin entrada".into())
    }
}

impl Tray for MvxTray {
    fn id(&self) -> String {
        "mvx-tray".into()
    }

    fn title(&self) -> String {
        "MVX".into()
    }

    fn icon_name(&self) -> String {
        if self.state.muted {
            "audio-input-microphone-muted".into()
        } else if self.state.running {
            "audio-input-microphone".into()
        } else {
            "audio-input-microphone-symbolic".into()
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
            title: "MVX · ModularVoiceX4yi".into(),
            description: format!(
                "{}<br/>{}<br/>{} Hz · quantum {}",
                self.status_text(),
                self.input_text(),
                self.state.sample_rate,
                self.state.quantum
            ),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = open_tui();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Abrir TUI".into(),
                icon_name: "utilities-terminal".into(),
                activate: Box::new(|_| {
                    let _ = open_tui();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: self.status_text(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Entrada: {}", self.input_text()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!(
                    "{} Hz · quantum {} · {:.1} ms est.",
                    self.state.sample_rate, self.state.quantum, self.state.latency_ms
                ),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Mute".into(),
                checked: self.state.muted,
                icon_name: "audio-volume-muted".into(),
                activate: Box::new(|this: &mut Self| {
                    let mute = !this.state.muted;
                    this.state.muted = mute;
                    let _ = send(Request::SetMute { mute });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Abrir micrófono virtual en Pavucontrol".into(),
                icon_name: "multimedia-volume-control".into(),
                activate: Box::new(|_| {
                    spawn_if_available("pavucontrol", &[]);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Cerrar tray".into(),
                icon_name: "window-close".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Apagar daemon".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| {
                    let _ = send(Request::Shutdown);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn fetch_state() -> Result<Snapshot> {
    let mut client = Client::connect(&socket_path()).context("el daemon no está iniciado")?;
    client.state()
}

fn send(request: Request) -> Result<()> {
    let mut client = Client::connect(&socket_path()).context("daemon no disponible")?;
    client.request(&request)?;
    Ok(())
}

fn open_tui() -> Result<()> {
    let mvx = find_binary("mvx").context("no se encontró el binario mvx en PATH")?;
    let mvx = mvx.to_string_lossy().to_string();
    let candidates: &[(&str, &[&str])] = &[
        ("xdg-terminal-exec", &[]),
        ("foot", &[]),
        ("kitty", &[]),
        ("alacritty", &["-e"]),
        ("wezterm", &["start", "--"]),
        ("konsole", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("xterm", &["-e"]),
    ];
    for (terminal, prefix) in candidates {
        if let Some(path) = find_binary(terminal) {
            let mut cmd = Command::new(path);
            cmd.args(*prefix).arg(&mvx).arg("tui");
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            cmd.spawn()
                .with_context(|| format!("no se pudo lanzar {terminal}"))?;
            return Ok(());
        }
    }
    anyhow::bail!("no hay un emulador de terminal compatible en PATH")
}

fn spawn_if_available(binary: &str, args: &[&str]) {
    if let Some(path) = find_binary(binary) {
        let _ = Command::new(path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

fn find_binary(name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
