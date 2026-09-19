use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::Frame;

use mvx_core::presets::FilterConfig;
use mvx_core::{socket_path, Preset, Quality, Snapshot};
use mvx_ipc::{Client, Request, Response};

#[derive(Debug, Parser)]
#[command(name = "mvx", about = "MVX voice modulator client")]
struct Args {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Open the interactive TUI.
    Tui,
    /// Print one state snapshot (use --waybar for Waybar custom module JSON).
    Status {
        /// Format as a single-line JSON for Waybar / Polybar module.
        #[arg(long)]
        waybar: bool,
    },
    /// Quick CLI control commands (toggle mute, bypass, monitor, presets, soundboard).
    Ctl {
        #[command(subcommand)]
        action: CtlAction,
    },
    /// List available input devices.
    Devices,
    /// Diagnose the daemon and audio path.
    Doctor,
    /// Manage speaker-verification models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Ask mvxd to exit.
    Shutdown,
}

#[derive(Debug, Subcommand)]
enum CtlAction {
    /// Toggle microphone mute.
    ToggleMute,
    /// Toggle DSP bypass (clean microphone).
    ToggleBypass,
    /// Toggle real-time Sidetone / monitor in headphones.
    ToggleMonitor,
    /// Set monitor volume (in dB).
    MonitorGain { gain_db: f32 },
    /// Apply a voice preset by its ID.
    Preset { id: String },
    /// Switch to the next preset.
    NextPreset,
    /// Switch to the previous preset.
    PrevPreset,
    /// Trigger a soundboard sound by its ID.
    Sound { id: String },
    /// Stop all playing soundboard sounds.
    StopSounds,
    /// Control the Voice Tester Loop (record, stop, toggle).
    LoopTest {
        #[arg(default_value = "toggle")]
        action: String,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsAction {
    /// Download the CAM++ speaker model and Silero VAD model.
    Install,
    /// Print the model file paths and verification status.
    Status,
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command.unwrap_or(CliCommand::Tui) {
        CliCommand::Tui => run_tui(),
        CliCommand::Status { waybar } => {
            let mut client = connect_or_start()?;
            let state = client.state()?;
            if waybar {
                let (text, alt, class) = if state.muted {
                    ("🔇 MUTE".to_string(), "muted", "muted")
                } else if state.bypass {
                    ("⚡ BYPASS".to_string(), "bypass", "warning")
                } else {
                    (format!("🎙️ {}", state.active_preset_name), "active", "active")
                };
                let tooltip = format!(
                    "ModularVoiceX4yi\nPreset: {}\nLatencia: {:.1} ms\nSidetone: {}\nBypass: {}",
                    state.active_preset_name,
                    state.total_latency_ms,
                    if state.monitor_enabled { "On" } else { "Off" },
                    if state.bypass { "On" } else { "Off" }
                );
                let json = serde_json::json!({
                    "text": text,
                    "alt": alt,
                    "tooltip": tooltip,
                    "class": class,
                    "percentage": if state.muted { 0 } else { 100 },
                });
                println!("{}", json);
            } else {
                println!("{}", serde_json::to_string_pretty(&state)?);
            }
            Ok(())
        }
        CliCommand::Ctl { action } => {
            let mut client = connect_or_start()?;
            match action {
                CtlAction::ToggleMute => {
                    let state = client.state()?;
                    client.set_mute(!state.muted)?;
                    println!("Mute: {}", if !state.muted { "activado" } else { "desactivado" });
                }
                CtlAction::ToggleBypass => {
                    let state = client.state()?;
                    client.set_bypass(!state.bypass)?;
                    println!("Bypass: {}", if !state.bypass { "activado" } else { "desactivado" });
                }
                CtlAction::ToggleMonitor => {
                    let state = client.state()?;
                    client.set_monitor(!state.monitor_enabled, state.monitor_gain_db)?;
                    println!("Sidetone / Monitor: {}", if !state.monitor_enabled { "activado" } else { "desactivado" });
                }
                CtlAction::MonitorGain { gain_db } => {
                    let state = client.state()?;
                    client.set_monitor(state.monitor_enabled, gain_db)?;
                    println!("Sidetone volumen: {:.1} dB", gain_db);
                }
                CtlAction::Preset { id } => {
                    client.apply_preset(id.clone())?;
                    println!("Preset aplicado: {}", id);
                }
                CtlAction::NextPreset => {
                    let state = client.state()?;
                    if !state.presets.is_empty() {
                        let cur_idx = state.presets.iter().position(|p| p.id == state.active_preset).unwrap_or(0);
                        let next_idx = (cur_idx + 1) % state.presets.len();
                        let next_id = &state.presets[next_idx].id;
                        client.apply_preset(next_id.clone())?;
                        println!("Preset: {}", state.presets[next_idx].name);
                    }
                }
                CtlAction::PrevPreset => {
                    let state = client.state()?;
                    if !state.presets.is_empty() {
                        let cur_idx = state.presets.iter().position(|p| p.id == state.active_preset).unwrap_or(0);
                        let prev_idx = if cur_idx == 0 { state.presets.len() - 1 } else { cur_idx - 1 };
                        let prev_id = &state.presets[prev_idx].id;
                        client.apply_preset(prev_id.clone())?;
                        println!("Preset: {}", state.presets[prev_idx].name);
                    }
                }
                CtlAction::Sound { id } => {
                    client.trigger_sound(id.clone())?;
                    println!("Reproduciendo sonido: {}", id);
                }
                CtlAction::StopSounds => {
                    client.stop_sounds()?;
                    println!("Sonidos detenidos");
                }
                CtlAction::LoopTest { action } => {
                    let state = client.state()?;
                    match action.to_lowercase().as_str() {
                        "record" | "rec" => {
                            client.start_loop_record()?;
                            println!("Loop tester: grabando 8s...");
                        }
                        "stop" => {
                            client.stop_loop_test()?;
                            println!("Loop tester: detenido");
                        }
                        _ => {
                            if state.loop_test_state == "idle" {
                                client.start_loop_record()?;
                                println!("Loop tester: grabando 8s...");
                            } else {
                                client.stop_loop_test()?;
                                println!("Loop tester: detenido");
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        CliCommand::Devices => {
            let mut client = connect_or_start()?;
            let state = client.state()?;
            if state.devices.is_empty() {
                println!("No hay entradas físicas visibles.");
            }
            for device in &state.devices {
                let marker = if state.selected_input.as_deref() == Some(device.name.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!("{marker} {:<45} {}", device.name, device.description);
            }
            Ok(())
        }
        CliCommand::Doctor => doctor(),
        CliCommand::Models { action } => models_command(action),
        CliCommand::Shutdown => {
            let mut client =
                Client::connect(&socket_path()).context("no se pudo conectar al daemon")?;
            let _ = client.request(&Request::Shutdown)?;
            println!("mvxd: apagándose");
            Ok(())
        }
    }
}

fn connect_or_start() -> Result<Client> {
    let path = socket_path();
    if let Ok(client) = Client::connect(&path) {
        return Ok(client);
    }

    if !mvx_core::is_instance_locked("mvxd.lock") {
        start_daemon()?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_err = None;
    while Instant::now() < deadline {
        match Client::connect(&path) {
            Ok(mut client) => {
                let _ = client.hello();
                return Ok(client);
            }
            Err(err) => last_err = Some(err),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow::anyhow!(
        "no se pudo contactar a mvxd en {}: {}",
        path.display(),
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "timeout".into())
    ))
}

fn start_daemon() -> Result<()> {
    let program = find_binary("mvxd")
        .context("no se encontró mvxd; instalalo en PATH o iniciá el servicio systemd")?;
    spawn_detached(&program, "mvxd.log").context("no se pudo iniciar mvxd")
}

fn start_tray() -> Result<()> {
    let program = find_binary("mvx-tray").context("no se encontró el binario mvx-tray")?;
    spawn_detached(&program, "mvx-tray.log").context("no se pudo iniciar mvx-tray")
}

fn spawn_detached(program: &Path, log_name: &str) -> Result<()> {
    let _ = mvx_core::ensure_runtime_dir();
    let log_path = mvx_core::runtime_dir().join(log_name);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let mut command = if Path::new("/usr/bin/setsid").exists() {
        let mut command = Command::new("/usr/bin/setsid");
        command.arg(program);
        command
    } else {
        Command::new(program)
    };
    command.stdin(Stdio::null());
    if let Some(log) = log {
        let log2 = log.try_clone().ok();
        command.stderr(log);
        if let Some(log2) = log2 {
            command.stdout(log2);
        }
    } else {
        command.stderr(Stdio::null()).stdout(Stdio::null());
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("no se pudo ejecutar {}", program.display()))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn find_binary(name: &str) -> Option<PathBuf> {
    if let Some(candidate) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|path| path.is_file())
    {
        return Some(candidate);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn run_tui() -> Result<()> {
    let mut app = App::new();
    let mut terminal = ratatui::init();
    match enable_mouse_capture() {
        Ok(()) => {
            app.mouse_active = true;
            app.status = "ratón listo: clic, arrastrá o usá la rueda".into();
        }
        Err(err) => {
            app.mouse_active = false;
            app.status = format!("ratón no disponible: {err}; usá el teclado");
        }
    }
    let result = app.run(&mut terminal);
    disable_mouse_capture();
    ratatui::restore();
    result
}

fn enable_mouse_capture() -> io::Result<()> {
    execute!(io::stdout(), EnableMouseCapture, EnableFocusChange)
}

fn disable_mouse_capture() {
    let _ = execute!(io::stdout(), DisableFocusChange, DisableMouseCapture);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Estudio,
    VoiceLab,
    Soundboard,
    Dispositivos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SliderKind {
    Master,
    PresetGain,
    MonitorGain,
    Noise,
    Threshold,
    #[allow(dead_code)]
    Hold,
    #[allow(dead_code)]
    Lookahead,
    Pitch,
    Formant,
    HpfCutoff,
    GateThreshold,
    EqLow,
    EqMid,
    EqHigh,
    CompThreshold,
    CompRatio,
    CompMakeup,
    ChorusMix,
    RobotFreq,
    RobotMix,
    RingFreq,
    RingMix,
    DistDrive,
    DistMix,
    ReverbRoom,
    ReverbMix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Tab(Tab),
    SelectPreset(String),
    ApplyPreset,
    NewPreset,
    ResetPreset,
    DeletePreset,
    ToggleMute,
    ToggleBypass,
    ToggleMonitor,
    ToggleLoopRecord,
    TriggerSound(String),
    StopSounds,
    Slider(SliderKind),
    Quality(Quality),
    ToggleFilter(&'static str),
    ToggleNoise,
    ToggleSpeaker,
    CycleProfile,
    DeleteVoice,
    EnrollVoice,
    InstallModels,
    Rescan,
    SelectDevice(usize),
    ModalSubmit,
    ModalCancel,
}

#[derive(Clone, Debug)]
struct Hitbox {
    rect: Rect,
    action: Action,
}

#[derive(Clone, Debug)]
struct Drag {
    kind: SliderKind,
    rect: Rect,
}

#[derive(Clone, Debug)]
enum Modal {
    NewPreset { value: String },
    EnrollVoice { value: String },
}

impl Modal {
    fn value(&self) -> &str {
        match self {
            Self::NewPreset { value } | Self::EnrollVoice { value } => value,
        }
    }

    fn value_mut(&mut self) -> &mut String {
        match self {
            Self::NewPreset { value } | Self::EnrollVoice { value } => value,
        }
    }

    fn title(&self) -> &'static str {
        match self {
            Self::NewPreset { .. } => "Nuevo preset",
            Self::EnrollVoice { .. } => "Enrolar mi voz",
        }
    }

    fn hint(&self) -> &'static str {
        match self {
            Self::NewPreset { .. } => {
                "Nombre del preset. Se crea una copia editable de los valores actuales."
            }
            Self::EnrollVoice { .. } => {
                "Nombre del perfil. Al confirmar, grabá 10 segundos de voz mirando al micrófono."
            }
        }
    }
}

struct App {
    client: Option<Client>,
    state: Snapshot,
    selected_preset: String,
    draft: Option<Preset>,
    needs_send: bool,
    last_send: Instant,
    master_edit: f32,
    master_dirty: bool,
    tab: Tab,
    selected_device: usize,
    selected_sound: usize,
    status: String,
    last_poll: Instant,
    last_start: Instant,
    last_edit: Instant,
    quit: bool,
    tray_checked: bool,
    hitboxes: Vec<Hitbox>,
    drag: Option<Drag>,
    hover: Option<Rect>,
    mouse_active: bool,
    modal: Option<Modal>,
}

impl App {
    fn new() -> Self {
        Self {
            client: None,
            state: Snapshot::initial(),
            selected_preset: "natural".into(),
            draft: None,
            needs_send: false,
            last_send: Instant::now() - Duration::from_secs(5),
            master_edit: 0.0,
            master_dirty: false,
            tab: Tab::Estudio,
            selected_device: 0,
            selected_sound: 0,
            status: "conectando…".into(),
            last_poll: Instant::now() - Duration::from_secs(10),
            last_start: Instant::now() - Duration::from_secs(10),
            last_edit: Instant::now() - Duration::from_secs(5),
            quit: false,
            tray_checked: false,
            hitboxes: Vec::new(),
            drag: None,
            hover: None,
            mouse_active: false,
            modal: None,
        }
    }

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        self.ensure_client();
        let mut last_mouse_reassert = Instant::now();
        while !self.quit {
            if self.mouse_active && last_mouse_reassert.elapsed() >= Duration::from_secs(5) {
                last_mouse_reassert = Instant::now();
                self.reassert_mouse();
            }
            if self.last_poll.elapsed() > Duration::from_millis(120) {
                self.poll();
                self.last_poll = Instant::now();
            }
            self.flush_edits(false);
            terminal.draw(|frame| self.draw(frame))?;

            if event::poll(Duration::from_millis(30))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key.code);
                        self.flush_edits(true);
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                        self.flush_edits(false);
                    }
                    Event::FocusGained | Event::Resize(_, _) => {
                        last_mouse_reassert = Instant::now();
                        self.reassert_mouse();
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn reassert_mouse(&mut self) {
        match enable_mouse_capture() {
            Ok(()) => {
                self.mouse_active = true;
            }
            Err(err) => {
                self.mouse_active = false;
                self.status = format!("ratón no disponible: {err}; usá el teclado");
            }
        }
    }

    fn ensure_client(&mut self) {
        if self.client.is_some() {
            return;
        }
        if self.last_start.elapsed() < Duration::from_secs(4) {
            return;
        }
        self.last_start = Instant::now();
        match connect_or_start() {
            Ok(mut client) => {
                if let Ok(state) = client.state() {
                    self.selected_preset = state.active_preset.clone();
                    self.selected_device = state
                        .devices
                        .iter()
                        .position(|d| Some(d.name.as_str()) == state.selected_input.as_deref())
                        .unwrap_or(0);
                    self.master_edit = state.gain_db;
                    self.state = state;
                }
                self.status = "conectado a mvxd".into();
                self.client = Some(client);
                self.ensure_tray();
            }
            Err(err) => {
                self.status = format!("sin daemon: {err}");
            }
        }
    }

    fn ensure_tray(&mut self) {
        if self.tray_checked || self.client.is_none() {
            return;
        }
        self.tray_checked = true;
        if let Err(err) = start_tray() {
            self.status = format!("tray no iniciado: {err}");
        }
    }

    fn poll(&mut self) {
        if self.client.is_none() {
            self.ensure_client();
            return;
        }
        let Some(client) = self.client.as_mut() else {
            return;
        };
        match client.state() {
            Ok(state) => {
                self.state = state;
                if self.state.error.is_some() {
                    self.status = self.state.error.clone().unwrap_or_default();
                }
                if self.selected_device >= self.state.devices.len() {
                    self.selected_device = self.state.devices.len().saturating_sub(1);
                }
                if self.selected_sound >= self.state.sound_clips.len() {
                    self.selected_sound = self.state.sound_clips.len().saturating_sub(1);
                }
                if !self.master_dirty {
                    self.master_edit = self.state.gain_db;
                }
                if self.draft.is_none()
                    || !self
                        .state
                        .presets
                        .iter()
                        .any(|preset| preset.id == self.selected_preset)
                {
                    self.selected_preset = self.state.active_preset.clone();
                    self.draft = self
                        .state
                        .presets
                        .iter()
                        .find(|preset| preset.id == self.selected_preset)
                        .cloned();
                } else if !self.needs_send && self.last_edit.elapsed() > Duration::from_millis(450)
                {
                    if let Some(current) = self
                        .state
                        .presets
                        .iter()
                        .find(|preset| preset.id == self.selected_preset)
                        .cloned()
                    {
                        self.draft = Some(current);
                    }
                }
            }
            Err(err) => {
                self.client = None;
                self.status = format!("daemon desconectado: {err}");
            }
        }
    }

    fn selected(&self) -> Option<&Preset> {
        self.draft
            .as_ref()
            .filter(|preset| preset.id == self.selected_preset)
            .or_else(|| {
                self.state
                    .presets
                    .iter()
                    .find(|preset| preset.id == self.selected_preset)
            })
    }

    fn selected_or_active(&self) -> Option<&Preset> {
        self.selected().or_else(|| {
            self.state
                .presets
                .iter()
                .find(|preset| preset.id == self.state.active_preset)
        })
    }

    fn ensure_draft(&mut self) -> Option<&mut Preset> {
        let needs_clone = self
            .draft
            .as_ref()
            .map(|preset| preset.id != self.selected_preset)
            .unwrap_or(true);
        if needs_clone {
            let found = self
                .state
                .presets
                .iter()
                .find(|preset| preset.id == self.selected_preset)
                .cloned();
            self.draft = found;
        }
        self.draft
            .as_mut()
            .filter(|preset| preset.id == self.selected_preset)
    }

    fn edit_selected<F: FnOnce(&mut Preset)>(&mut self, edit: F) {
        if let Some(preset) = self.ensure_draft() {
            edit(preset);
            preset.normalize();
            self.needs_send = true;
            self.last_edit = Instant::now();
        }
    }

    fn flush_edits(&mut self, force: bool) {
        if self.master_dirty {
            self.master_dirty = false;
            self.send(Request::SetGain {
                gain_db: self.master_edit,
            });
        }
        if !self.needs_send {
            return;
        }
        let throttle = if self.drag.is_some() {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(45)
        };
        if !force && self.last_send.elapsed() < throttle {
            return;
        }
        let Some(preset) = self.selected().cloned() else {
            return;
        };
        self.last_send = Instant::now();
        self.needs_send = false;
        self.send(Request::UpsertPreset {
            preset,
            activate: true,
        });
    }

    fn send(&mut self, request: Request) {
        let Some(client) = self.client.as_mut() else {
            self.status = "sin daemon".into();
            return;
        };
        match client.request(&request) {
            Ok(Response::Error { message }) => self.status = message,
            Ok(_) => {}
            Err(err) => {
                self.client = None;
                self.status = format!("error IPC: {err}");
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        if self.modal.is_some() {
            self.handle_modal_key(code);
            return;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                self.tab = match self.tab {
                    Tab::Estudio => Tab::VoiceLab,
                    Tab::VoiceLab => Tab::Soundboard,
                    Tab::Soundboard => Tab::Dispositivos,
                    Tab::Dispositivos => Tab::Estudio,
                };
            }
            KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Estudio => Tab::Dispositivos,
                    Tab::VoiceLab => Tab::Estudio,
                    Tab::Soundboard => Tab::VoiceLab,
                    Tab::Dispositivos => Tab::Soundboard,
                };
            }
            KeyCode::F(1) => self.tab = Tab::Estudio,
            KeyCode::F(2) => self.tab = Tab::VoiceLab,
            KeyCode::F(3) => self.tab = Tab::Soundboard,
            KeyCode::F(4) => self.tab = Tab::Dispositivos,

            KeyCode::Char('1') if self.tab != Tab::Soundboard => self.tab = Tab::Estudio,
            KeyCode::Char('2') if self.tab != Tab::Soundboard => self.tab = Tab::VoiceLab,
            KeyCode::Char('3') if self.tab != Tab::Soundboard => self.tab = Tab::Soundboard,
            KeyCode::Char('4') if self.tab != Tab::Soundboard => self.tab = Tab::Dispositivos,

            KeyCode::Char(c @ '1'..='9') if self.tab == Tab::Soundboard => {
                let idx = (c as u8 - b'1') as usize;
                if let Some(clip) = self.state.sound_clips.get(idx) {
                    let id = clip.id.clone();
                    self.send(Request::TriggerSound { id: id.clone() });
                    self.status = format!("reproduciendo: {id}");
                }
            }

            KeyCode::Up | KeyCode::Char('k') => match self.tab {
                Tab::Estudio | Tab::VoiceLab => self.move_preset(-1),
                Tab::Soundboard => self.move_sound(-1),
                Tab::Dispositivos => {
                    self.selected_device = self.selected_device.saturating_sub(1);
                }
            },
            KeyCode::Down | KeyCode::Char('j') => match self.tab {
                Tab::Estudio | Tab::VoiceLab => self.move_preset(1),
                Tab::Soundboard => self.move_sound(1),
                Tab::Dispositivos => {
                    if !self.state.devices.is_empty() {
                        self.selected_device =
                            (self.selected_device + 1).min(self.state.devices.len() - 1);
                    }
                }
            },
            KeyCode::Enter => match self.tab {
                Tab::Estudio | Tab::VoiceLab => self.apply_selected(),
                Tab::Soundboard => {
                    if let Some(clip) = self.state.sound_clips.get(self.selected_sound) {
                        let id = clip.id.clone();
                        self.send(Request::TriggerSound { id: id.clone() });
                        self.status = format!("reproduciendo: {id}");
                    }
                }
                Tab::Dispositivos => self.select_device(self.selected_device),
            },
            KeyCode::Char(' ') => self.stop_sounds(),
            KeyCode::Char('n') => self.open_new_preset(),
            KeyCode::Char('r') => self.reset_selected(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('b') => self.toggle_bypass(),
            KeyCode::Char('s') => self.toggle_monitor(),
            KeyCode::Char('v') => self.toggle_speaker(),
            KeyCode::Char('l') => self.toggle_loop_test(),
            KeyCode::Char('e') => self.open_enroll(),
            KeyCode::Char('i') => self.install_models(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.bump_master(1.0),
            KeyCode::Char('-') | KeyCode::Char('_') => self.bump_master(-1.0),
            KeyCode::Char(']') => self.bump_pitch(0.5),
            KeyCode::Char('[') => self.bump_pitch(-0.5),
            _ => {}
        }
    }

    fn handle_modal_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.modal = None,
            KeyCode::Enter => self.submit_modal(),
            KeyCode::Backspace => {
                if let Some(modal) = self.modal.as_mut() {
                    modal.value_mut().pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(modal) = self.modal.as_mut() {
                    if modal.value().len() < 64 {
                        modal.value_mut().push(c);
                    }
                }
            }
            _ => {}
        }
    }

    fn submit_modal(&mut self) {
        let value = self
            .modal
            .as_ref()
            .map(|modal| modal.value().trim().to_string())
            .unwrap_or_default();
        if value.is_empty() {
            self.status = "el nombre no puede estar vacío".into();
            return;
        }
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal {
            Modal::NewPreset { .. } => {
                let base = self
                    .selected_or_active()
                    .cloned()
                    .unwrap_or_else(|| self.state.presets[0].clone());
                let id = self.next_custom_id();
                let mut preset = base;
                preset.id = id;
                preset.name = value;
                preset.builtin = false;
                self.selected_preset = preset.id.clone();
                self.draft = Some(preset.clone());
                self.needs_send = false;
                self.send(Request::UpsertPreset {
                    preset,
                    activate: true,
                });
                self.status = "preset personalizado creado".into();
            }
            Modal::EnrollVoice { .. } => {
                let Some(mut preset) = self.selected_or_active().cloned() else {
                    self.status = "no hay un preset activo".into();
                    return;
                };
                if let Err(err) = mvx_core::validate_profile_name(&value) {
                    self.status = format!("nombre de perfil inválido: {err}");
                    return;
                }
                if !preset.has_filter("speaker_lock") {
                    preset.add_filter("speaker_lock");
                }
                preset.filters.iter_mut().for_each(|filter| {
                    if let FilterConfig::SpeakerLock {
                        enabled, profile, ..
                    } = filter
                    {
                        *enabled = true;
                        *profile = value.clone();
                    }
                });
                self.selected_preset = preset.id.clone();
                self.draft = Some(preset.clone());
                self.needs_send = false;
                self.send(Request::UpsertPreset {
                    preset,
                    activate: true,
                });
                self.send(Request::EnrollVoice {
                    name: value,
                    seconds: 10,
                });
                self.status = "enrolamiento iniciado (10s)".into();
            }
        }
    }

    fn next_custom_id(&self) -> String {
        let mut n = 1;
        loop {
            let id = format!("custom_{n}");
            if !self.state.presets.iter().any(|preset| preset.id == id) {
                return id;
            }
            n += 1;
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let point = Rect::new(mouse.column, mouse.row, 1, 1);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = self.hit_at(point) {
                    if self.modal.is_some()
                        && !matches!(hit.action, Action::ModalSubmit | Action::ModalCancel)
                    {
                        return;
                    }
                    if let Action::Slider(kind) = hit.action {
                        self.drag = Some(Drag {
                            kind,
                            rect: hit.rect,
                        });
                        self.apply_slider(kind, hit.rect, mouse.column);
                    } else {
                        self.perform(hit.action);
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(drag) = self.drag.clone() {
                    self.apply_slider(drag.kind, drag.rect, mouse.column);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                self.flush_edits(true);
            }
            MouseEventKind::Moved => {
                self.hover = self.hit_at(point).map(|hit| hit.rect);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some(hit) = self.hit_at(point) {
                    if let Action::Slider(kind) = hit.action {
                        let delta = if mouse.kind == MouseEventKind::ScrollUp {
                            1.0
                        } else {
                            -1.0
                        };
                        self.adjust_slider(kind, delta);
                    } else if matches!(self.tab, Tab::Estudio | Tab::VoiceLab) {
                        self.move_preset(if mouse.kind == MouseEventKind::ScrollUp {
                            -1
                        } else {
                            1
                        });
                    } else if self.tab == Tab::Soundboard {
                        self.move_sound(if mouse.kind == MouseEventKind::ScrollUp {
                            -1
                        } else {
                            1
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn hit_at(&self, point: Rect) -> Option<Hitbox> {
        self.hitboxes
            .iter()
            .rev()
            .find(|hit| hit.rect.contains(point.into()))
            .cloned()
    }

    fn register(&mut self, rect: Rect, action: Action) {
        self.hitboxes.push(Hitbox { rect, action });
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Tab(tab) => self.tab = tab,
            Action::SelectPreset(id) => {
                self.flush_edits(true);
                self.selected_preset = id.clone();
                self.draft = self
                    .state
                    .presets
                    .iter()
                    .find(|preset| preset.id == id)
                    .cloned();
                self.needs_send = false;
                self.send(Request::ApplyPreset { id: id.clone() });
                self.status = format!("preset: {id}");
            }
            Action::ApplyPreset => self.apply_selected(),
            Action::NewPreset => {
                self.flush_edits(true);
                self.open_new_preset();
            }
            Action::ResetPreset => {
                self.flush_edits(true);
                self.reset_selected();
            }
            Action::DeletePreset => {
                self.flush_edits(true);
                self.delete_selected();
            }
            Action::ToggleMute => self.toggle_mute(),
            Action::ToggleBypass => self.toggle_bypass(),
            Action::ToggleMonitor => self.toggle_monitor(),
            Action::ToggleLoopRecord => self.toggle_loop_test(),
            Action::TriggerSound(id) => {
                self.send(Request::TriggerSound { id: id.clone() });
                self.status = format!("reproduciendo: {id}");
            }
            Action::StopSounds => self.stop_sounds(),
            Action::Slider(kind) => {
                self.adjust_slider(kind, 0.0);
            }
            Action::Quality(quality) => {
                self.edit_selected(|preset| preset.quality = quality);
                self.status = format!("calidad DSP: {}", quality.label());
            }
            Action::ToggleFilter(kind) => {
                self.edit_selected(|preset| {
                    let was_present = preset.has_filter(kind);
                    if !was_present {
                        preset.add_filter(kind);
                    } else {
                        preset.filters.iter_mut().for_each(|f| {
                            if f.kind() == kind {
                                f.set_enabled(!f.enabled());
                            }
                        });
                    }
                });
            }
            Action::ToggleNoise => self.toggle_noise(),
            Action::ToggleSpeaker => self.toggle_speaker(),
            Action::CycleProfile => self.cycle_profile(),
            Action::DeleteVoice => self.delete_profile(),
            Action::EnrollVoice => self.open_enroll(),
            Action::InstallModels => self.install_models(),
            Action::Rescan => {
                self.send(Request::GetState);
                self.status = "dispositivos actualizados".into();
            }
            Action::SelectDevice(index) => self.select_device(index),
            Action::ModalSubmit => self.submit_modal(),
            Action::ModalCancel => self.modal = None,
        }
    }

    fn apply_slider(&mut self, kind: SliderKind, rect: Rect, x: u16) {
        let width = rect.width.saturating_sub(2).max(1);
        let rel = x.saturating_sub(rect.x + 1).min(width) as f32 / width as f32;
        match kind {
            SliderKind::Master => {
                let value = -24.0 + rel * 48.0;
                self.set_master(value.round());
            }
            SliderKind::PresetGain => {
                let value = -24.0 + rel * 48.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("gain") {
                        preset.add_filter("gain");
                    }
                    preset.filters.iter_mut().for_each(|filter| {
                        if let FilterConfig::Gain { enabled, gain_db } = filter {
                            *enabled = true;
                            *gain_db = value.round();
                        }
                    });
                });
            }
            SliderKind::MonitorGain => {
                let value = -24.0 + rel * 36.0;
                let val = value.round();
                let enabled = self.state.monitor_enabled;
                self.send(Request::SetMonitor {
                    enabled,
                    gain_db: val,
                });
            }
            SliderKind::Noise => {
                let value = rel;
                self.edit_selected(|preset| {
                    if !preset.has_filter("noise_reduction") {
                        preset.add_filter("noise_reduction");
                    }
                    preset.filters.iter_mut().for_each(|filter| {
                        if let FilterConfig::NoiseReduction { enabled, strength } = filter {
                            *enabled = true;
                            *strength = value;
                        }
                    });
                });
            }
            SliderKind::Threshold => {
                let value = 0.50 + rel * 0.45;
                self.edit_selected(|preset| {
                    if !preset.has_filter("speaker_lock") {
                        preset.add_filter("speaker_lock");
                    }
                    preset.filters.iter_mut().for_each(|filter| {
                        if let FilterConfig::SpeakerLock { threshold, .. } = filter {
                            *threshold = value;
                        }
                    });
                });
            }
            SliderKind::Hold => {
                let value = 50.0 + rel * 1_450.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("speaker_lock") {
                        preset.add_filter("speaker_lock");
                    }
                    preset.filters.iter_mut().for_each(|filter| {
                        if let FilterConfig::SpeakerLock { hold_ms, .. } = filter {
                            *hold_ms = (value / 50.0).round() as u32 * 50;
                        }
                    });
                });
            }
            SliderKind::Lookahead => {
                let value = rel * 300.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("speaker_lock") {
                        preset.add_filter("speaker_lock");
                    }
                    preset.filters.iter_mut().for_each(|filter| {
                        if let FilterConfig::SpeakerLock { lookahead_ms, .. } = filter {
                            *lookahead_ms = (value / 25.0).round() as u32 * 25;
                        }
                    });
                });
            }
            SliderKind::Pitch => {
                let value = -12.0 + rel * 24.0;
                self.edit_selected(|preset| preset.pitch_semitones = (value * 10.0).round() / 10.0);
            }
            SliderKind::Formant => {
                let value = -6.0 + rel * 12.0;
                self.edit_selected(|preset| {
                    preset.formant_semitones = (value * 10.0).round() / 10.0
                });
            }
            SliderKind::HpfCutoff => {
                let value = 20.0 + rel * 280.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("high_pass") {
                        preset.add_filter("high_pass");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::HighPass {
                            cutoff_hz, enabled, ..
                        } = f
                        {
                            *enabled = true;
                            *cutoff_hz = (value / 5.0).round() * 5.0;
                        }
                    });
                });
            }
            SliderKind::GateThreshold => {
                let value = -60.0 + rel * 60.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("noise_gate") {
                        preset.add_filter("noise_gate");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::NoiseGate {
                            threshold_db,
                            enabled,
                            ..
                        } = f
                        {
                            *enabled = true;
                            *threshold_db = value.round();
                        }
                    });
                });
            }
            SliderKind::EqLow => {
                let value = -15.0 + rel * 30.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("equalizer") {
                        preset.add_filter("equalizer");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Equalizer { low_db, enabled, .. } = f {
                            *enabled = true;
                            *low_db = value.round();
                        }
                    });
                });
            }
            SliderKind::EqMid => {
                let value = -15.0 + rel * 30.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("equalizer") {
                        preset.add_filter("equalizer");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Equalizer { mid_db, enabled, .. } = f {
                            *enabled = true;
                            *mid_db = value.round();
                        }
                    });
                });
            }
            SliderKind::EqHigh => {
                let value = -15.0 + rel * 30.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("equalizer") {
                        preset.add_filter("equalizer");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Equalizer {
                            high_db, enabled, ..
                        } = f
                        {
                            *enabled = true;
                            *high_db = value.round();
                        }
                    });
                });
            }
            SliderKind::CompThreshold => {
                let value = -40.0 + rel * 40.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("compressor") {
                        preset.add_filter("compressor");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Compressor {
                            threshold_db,
                            enabled,
                            ..
                        } = f
                        {
                            *enabled = true;
                            *threshold_db = value.round();
                        }
                    });
                });
            }
            SliderKind::CompRatio => {
                let value = 1.0 + rel * 9.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("compressor") {
                        preset.add_filter("compressor");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Compressor { ratio, enabled, .. } = f {
                            *enabled = true;
                            *ratio = (value * 10.0).round() / 10.0;
                        }
                    });
                });
            }
            SliderKind::CompMakeup => {
                let value = rel * 24.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("compressor") {
                        preset.add_filter("compressor");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Compressor {
                            makeup_db, enabled, ..
                        } = f
                        {
                            *enabled = true;
                            *makeup_db = value.round();
                        }
                    });
                });
            }
            SliderKind::ChorusMix => {
                let value = rel;
                self.edit_selected(|preset| {
                    if !preset.has_filter("chorus") {
                        preset.add_filter("chorus");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Chorus { mix, enabled, .. } = f {
                            *enabled = true;
                            *mix = (value * 100.0).round() / 100.0;
                        }
                    });
                });
            }
            SliderKind::RobotFreq => {
                let value = 20.0 + rel * 480.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("robotizer") {
                        preset.add_filter("robotizer");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Robotizer {
                            frequency_hz,
                            enabled,
                            ..
                        } = f
                        {
                            *enabled = true;
                            *frequency_hz = (value / 5.0).round() * 5.0;
                        }
                    });
                });
            }
            SliderKind::RobotMix => {
                let value = rel;
                self.edit_selected(|preset| {
                    if !preset.has_filter("robotizer") {
                        preset.add_filter("robotizer");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Robotizer { mix, enabled, .. } = f {
                            *enabled = true;
                            *mix = (value * 100.0).round() / 100.0;
                        }
                    });
                });
            }
            SliderKind::RingFreq => {
                let value = 50.0 + rel * 1950.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("ring_mod") {
                        preset.add_filter("ring_mod");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::RingMod {
                            frequency_hz,
                            enabled,
                            ..
                        } = f
                        {
                            *enabled = true;
                            *frequency_hz = (value / 10.0).round() * 10.0;
                        }
                    });
                });
            }
            SliderKind::RingMix => {
                let value = rel;
                self.edit_selected(|preset| {
                    if !preset.has_filter("ring_mod") {
                        preset.add_filter("ring_mod");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::RingMod { mix, enabled, .. } = f {
                            *enabled = true;
                            *mix = (value * 100.0).round() / 100.0;
                        }
                    });
                });
            }
            SliderKind::DistDrive => {
                let value = rel * 24.0;
                self.edit_selected(|preset| {
                    if !preset.has_filter("distortion") {
                        preset.add_filter("distortion");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Distortion { drive, enabled, .. } = f {
                            *enabled = true;
                            *drive = value.round();
                        }
                    });
                });
            }
            SliderKind::DistMix => {
                let value = rel;
                self.edit_selected(|preset| {
                    if !preset.has_filter("distortion") {
                        preset.add_filter("distortion");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Distortion { mix, enabled, .. } = f {
                            *enabled = true;
                            *mix = (value * 100.0).round() / 100.0;
                        }
                    });
                });
            }
            SliderKind::ReverbRoom => {
                let value = rel;
                self.edit_selected(|preset| {
                    if !preset.has_filter("reverb") {
                        preset.add_filter("reverb");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Reverb {
                            room_size, enabled, ..
                        } = f
                        {
                            *enabled = true;
                            *room_size = (value * 100.0).round() / 100.0;
                        }
                    });
                });
            }
            SliderKind::ReverbMix => {
                let value = rel;
                self.edit_selected(|preset| {
                    if !preset.has_filter("reverb") {
                        preset.add_filter("reverb");
                    }
                    preset.filters.iter_mut().for_each(|f| {
                        if let FilterConfig::Reverb { mix, enabled, .. } = f {
                            *enabled = true;
                            *mix = (value * 100.0).round() / 100.0;
                        }
                    });
                });
            }
        }
    }

    fn adjust_slider(&mut self, kind: SliderKind, direction: f32) {
        match kind {
            SliderKind::Master => self.bump_master(direction),
            SliderKind::PresetGain => self.bump_preset_gain(direction),
            SliderKind::MonitorGain => {
                let val = (self.state.monitor_gain_db + direction * 1.0).clamp(-24.0, 12.0);
                self.send(Request::SetMonitor {
                    enabled: self.state.monitor_enabled,
                    gain_db: val,
                });
            }
            SliderKind::Noise => self.edit_selected(|preset| {
                if let Some(FilterConfig::NoiseReduction { strength, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::NoiseReduction { .. }))
                {
                    *strength = (*strength + direction * 0.05).clamp(0.0, 1.0);
                }
            }),
            SliderKind::Threshold => self.edit_selected(|preset| {
                if let Some(FilterConfig::SpeakerLock { threshold, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::SpeakerLock { .. }))
                {
                    *threshold = (*threshold + direction * 0.02).clamp(0.5, 0.99);
                }
            }),
            SliderKind::Hold => self.edit_selected(|preset| {
                if let Some(FilterConfig::SpeakerLock { hold_ms, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::SpeakerLock { .. }))
                {
                    *hold_ms = (*hold_ms).saturating_add_signed((direction * 50.0) as i32);
                    *hold_ms = (*hold_ms).clamp(50, 1_500);
                }
            }),
            SliderKind::Lookahead => self.edit_selected(|preset| {
                if let Some(FilterConfig::SpeakerLock { lookahead_ms, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::SpeakerLock { .. }))
                {
                    *lookahead_ms =
                        (*lookahead_ms).saturating_add_signed((direction * 25.0) as i32);
                    *lookahead_ms = (*lookahead_ms).min(300);
                }
            }),
            SliderKind::Pitch => self.bump_pitch(direction * 0.5),
            SliderKind::Formant => self.edit_selected(|preset| {
                preset.formant_semitones =
                    (preset.formant_semitones + direction * 0.5).clamp(-6.0, 6.0)
            }),
            SliderKind::HpfCutoff => self.edit_selected(|preset| {
                if let Some(FilterConfig::HighPass { cutoff_hz, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::HighPass { .. }))
                {
                    *cutoff_hz = (*cutoff_hz + direction * 10.0).clamp(20.0, 300.0);
                }
            }),
            SliderKind::GateThreshold => self.edit_selected(|preset| {
                if let Some(FilterConfig::NoiseGate { threshold_db, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::NoiseGate { .. }))
                {
                    *threshold_db = (*threshold_db + direction * 1.0).clamp(-60.0, 0.0);
                }
            }),
            SliderKind::EqLow => self.edit_selected(|preset| {
                if let Some(FilterConfig::Equalizer { low_db, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Equalizer { .. }))
                {
                    *low_db = (*low_db + direction * 1.0).clamp(-15.0, 15.0);
                }
            }),
            SliderKind::EqMid => self.edit_selected(|preset| {
                if let Some(FilterConfig::Equalizer { mid_db, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Equalizer { .. }))
                {
                    *mid_db = (*mid_db + direction * 1.0).clamp(-15.0, 15.0);
                }
            }),
            SliderKind::EqHigh => self.edit_selected(|preset| {
                if let Some(FilterConfig::Equalizer { high_db, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Equalizer { .. }))
                {
                    *high_db = (*high_db + direction * 1.0).clamp(-15.0, 15.0);
                }
            }),
            SliderKind::CompThreshold => self.edit_selected(|preset| {
                if let Some(FilterConfig::Compressor { threshold_db, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Compressor { .. }))
                {
                    *threshold_db = (*threshold_db + direction * 1.0).clamp(-40.0, 0.0);
                }
            }),
            SliderKind::CompRatio => self.edit_selected(|preset| {
                if let Some(FilterConfig::Compressor { ratio, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Compressor { .. }))
                {
                    *ratio = (*ratio + direction * 0.5).clamp(1.0, 10.0);
                }
            }),
            SliderKind::CompMakeup => self.edit_selected(|preset| {
                if let Some(FilterConfig::Compressor { makeup_db, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Compressor { .. }))
                {
                    *makeup_db = (*makeup_db + direction * 1.0).clamp(0.0, 24.0);
                }
            }),
            SliderKind::ChorusMix => self.edit_selected(|preset| {
                if let Some(FilterConfig::Chorus { mix, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Chorus { .. }))
                {
                    *mix = (*mix + direction * 0.05).clamp(0.0, 1.0);
                }
            }),
            SliderKind::RobotFreq => self.edit_selected(|preset| {
                if let Some(FilterConfig::Robotizer { frequency_hz, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Robotizer { .. }))
                {
                    *frequency_hz = (*frequency_hz + direction * 10.0).clamp(20.0, 500.0);
                }
            }),
            SliderKind::RobotMix => self.edit_selected(|preset| {
                if let Some(FilterConfig::Robotizer { mix, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Robotizer { .. }))
                {
                    *mix = (*mix + direction * 0.05).clamp(0.0, 1.0);
                }
            }),
            SliderKind::RingFreq => self.edit_selected(|preset| {
                if let Some(FilterConfig::RingMod { frequency_hz, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::RingMod { .. }))
                {
                    *frequency_hz = (*frequency_hz + direction * 25.0).clamp(50.0, 2000.0);
                }
            }),
            SliderKind::RingMix => self.edit_selected(|preset| {
                if let Some(FilterConfig::RingMod { mix, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::RingMod { .. }))
                {
                    *mix = (*mix + direction * 0.05).clamp(0.0, 1.0);
                }
            }),
            SliderKind::DistDrive => self.edit_selected(|preset| {
                if let Some(FilterConfig::Distortion { drive, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Distortion { .. }))
                {
                    *drive = (*drive + direction * 1.0).clamp(0.0, 24.0);
                }
            }),
            SliderKind::DistMix => self.edit_selected(|preset| {
                if let Some(FilterConfig::Distortion { mix, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Distortion { .. }))
                {
                    *mix = (*mix + direction * 0.05).clamp(0.0, 1.0);
                }
            }),
            SliderKind::ReverbRoom => self.edit_selected(|preset| {
                if let Some(FilterConfig::Reverb { room_size, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Reverb { .. }))
                {
                    *room_size = (*room_size + direction * 0.05).clamp(0.0, 1.0);
                }
            }),
            SliderKind::ReverbMix => self.edit_selected(|preset| {
                if let Some(FilterConfig::Reverb { mix, .. }) = preset
                    .filters
                    .iter_mut()
                    .find(|f| matches!(f, FilterConfig::Reverb { .. }))
                {
                    *mix = (*mix + direction * 0.05).clamp(0.0, 1.0);
                }
            }),
        }
    }

    fn set_master(&mut self, value: f32) {
        self.master_edit = value.clamp(-24.0, 24.0);
        self.master_dirty = true;
        self.last_edit = Instant::now();
    }

    fn bump_master(&mut self, delta: f32) {
        self.set_master(self.master_edit + delta);
    }

    fn bump_preset_gain(&mut self, delta: f32) {
        self.edit_selected(|preset| {
            if !preset.has_filter("gain") {
                preset.add_filter("gain");
            }
            preset.filters.iter_mut().for_each(|filter| {
                if let FilterConfig::Gain {
                    enabled, gain_db, ..
                } = filter
                {
                    *enabled = true;
                    *gain_db = (*gain_db + delta).clamp(-24.0, 24.0);
                }
            });
        });
    }

    fn bump_pitch(&mut self, delta: f32) {
        self.edit_selected(|preset| {
            preset.pitch_semitones = (preset.pitch_semitones + delta).clamp(-12.0, 12.0);
        });
    }

    fn move_preset(&mut self, delta: i32) {
        self.flush_edits(true);
        if self.state.presets.is_empty() {
            return;
        }
        let index = self
            .state
            .presets
            .iter()
            .position(|preset| preset.id == self.selected_preset)
            .unwrap_or(0) as i32;
        let next = (index + delta).clamp(0, self.state.presets.len() as i32 - 1) as usize;
        let id = self.state.presets[next].id.clone();
        self.selected_preset = id;
        self.draft = Some(self.state.presets[next].clone());
        self.needs_send = false;
    }

    fn move_sound(&mut self, delta: i32) {
        if self.state.sound_clips.is_empty() {
            return;
        }
        let next = (self.selected_sound as i32 + delta)
            .clamp(0, self.state.sound_clips.len() as i32 - 1) as usize;
        self.selected_sound = next;
    }

    fn apply_selected(&mut self) {
        let id = self.selected_preset.clone();
        self.send(Request::ApplyPreset { id });
        self.status = "preset aplicado".into();
    }

    fn open_new_preset(&mut self) {
        self.flush_edits(true);
        self.modal = Some(Modal::NewPreset {
            value: String::new(),
        });
    }

    fn open_enroll(&mut self) {
        self.flush_edits(true);
        if !self.state.models_installed {
            self.status = "instalá primero los modelos de voz (~30 MB)".into();
            return;
        }
        self.modal = Some(Modal::EnrollVoice {
            value: "mi_voz".into(),
        });
    }

    fn reset_selected(&mut self) {
        let Some(preset) = self.selected() else {
            return;
        };
        if !preset.builtin {
            self.status = "solo los presets built-in se pueden restaurar".into();
            return;
        }
        let id = preset.id.clone();
        self.send(Request::ResetPreset { id });
        self.draft = None;
        self.status = "preset restaurado".into();
    }

    fn delete_selected(&mut self) {
        let Some(preset) = self.selected() else {
            return;
        };
        if preset.builtin {
            self.status = "un preset built-in no se puede borrar; usá Restaurar".into();
            return;
        }
        let id = preset.id.clone();
        self.send(Request::DeletePreset { id });
        self.selected_preset = self.state.active_preset.clone();
        self.draft = None;
        self.status = "preset eliminado".into();
    }

    fn toggle_mute(&mut self) {
        let mute = !self.state.muted;
        self.send(Request::SetMute { mute });
        self.status = if mute {
            "mute ON".into()
        } else {
            "mute OFF".into()
        };
    }

    fn toggle_bypass(&mut self) {
        let bypass = !self.state.bypass;
        self.send(Request::SetBypass { bypass });
        self.status = if bypass {
            "bypass ON (micrófono limpio)".into()
        } else {
            "bypass OFF (DSP activo)".into()
        };
    }

    fn toggle_monitor(&mut self) {
        let enabled = !self.state.monitor_enabled;
        self.send(Request::SetMonitor {
            enabled,
            gain_db: self.state.monitor_gain_db,
        });
        self.status = if enabled {
            "sidetone / monitor ON".into()
        } else {
            "sidetone / monitor OFF".into()
        };
    }

    fn toggle_loop_test(&mut self) {
        if self.state.loop_test_state == "idle" {
            self.send(Request::StartLoopRecord);
            self.status = "loop tester: grabando 8s...".into();
        } else {
            self.send(Request::StopLoopTest);
            self.status = "loop tester: detenido".into();
        }
    }

    fn stop_sounds(&mut self) {
        self.send(Request::StopSounds);
        self.status = "sonidos detenidos".into();
    }

    fn toggle_noise(&mut self) {
        self.edit_selected(|preset| {
            let was_present = preset.has_filter("noise_reduction");
            if !was_present {
                preset.add_filter("noise_reduction");
            } else {
                preset.filters.iter_mut().for_each(|filter| {
                    if let FilterConfig::NoiseReduction { enabled, .. } = filter {
                        *enabled = !*enabled;
                    }
                });
            }
        });
    }

    fn toggle_speaker(&mut self) {
        let profiles = self.state.voice_profiles.clone();
        self.edit_selected(|preset| {
            let was_present = preset.has_filter("speaker_lock");
            if !was_present {
                preset.add_filter("speaker_lock");
            }
            let profile = if profiles.is_empty() {
                String::new()
            } else {
                profiles[0].clone()
            };
            preset.filters.iter_mut().for_each(|filter| {
                if let FilterConfig::SpeakerLock {
                    enabled,
                    profile: p,
                    ..
                } = filter
                {
                    if was_present {
                        *enabled = !*enabled;
                    }
                    if *enabled && p.is_empty() {
                        *p = profile.clone();
                    }
                }
            });
        });
        let enabled = self
            .selected_or_active()
            .and_then(|preset| preset.speaker_lock())
            .is_some();
        if enabled && self.state.voice_profiles.is_empty() {
            self.status = "enrolá tu voz antes de activar Solo mi voz".into();
        } else if enabled && !self.state.models_installed {
            self.status = "instalá los modelos de voz para usar Solo mi voz".into();
        } else {
            self.status = if enabled {
                "Solo mi voz ON".into()
            } else {
                "Solo mi voz OFF".into()
            };
        }
    }

    fn cycle_profile(&mut self) {
        let profiles = self.state.voice_profiles.clone();
        if profiles.is_empty() {
            self.status = "no hay perfiles; usá Enrolar mi voz".into();
            return;
        }
        self.edit_selected(|preset| {
            if !preset.has_filter("speaker_lock") {
                preset.add_filter("speaker_lock");
            }
            preset.filters.iter_mut().for_each(|filter| {
                if let FilterConfig::SpeakerLock { profile, .. } = filter {
                    let next = profiles
                        .iter()
                        .position(|name| name == profile)
                        .map(|index| (index + 1) % profiles.len())
                        .unwrap_or(0);
                    *profile = profiles[next].clone();
                }
            });
        });
        self.status = "perfil de voz cambiado".into();
    }

    fn delete_profile(&mut self) {
        let Some(profile) = self
            .selected_or_active()
            .and_then(|preset| preset.speaker_lock())
            .map(|lock| lock.profile)
            .filter(|profile| !profile.is_empty())
        else {
            self.status = "no hay perfil seleccionado".into();
            return;
        };
        self.send(Request::DeleteVoice {
            name: profile.clone(),
        });
        self.status = format!("perfil «{profile}» eliminado");
    }

    fn install_models(&mut self) {
        self.send(Request::InstallModels);
        self.status = "descargando modelos de voz…".into();
    }

    fn select_device(&mut self, index: usize) {
        if let Some(device) = self.state.devices.get(index) {
            let name = device.name.clone();
            let description = device.description.clone();
            self.selected_device = index;
            self.send(Request::SetInput { name });
            self.status = format!("entrada: {description}");
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        self.hitboxes.clear();
        let area = frame.area();
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(2),
            ])
            .split(area);

        self.draw_header(frame, outer[0]);
        match self.tab {
            Tab::Estudio => self.draw_estudio(frame, outer[1]),
            Tab::VoiceLab => self.draw_voice_lab(frame, outer[1]),
            Tab::Soundboard => self.draw_soundboard(frame, outer[1]),
            Tab::Dispositivos => self.draw_devices(frame, outer[1]),
        }
        self.draw_footer(frame, outer[2]);
        if self.modal.is_some() {
            self.draw_modal(frame);
        }
    }

    fn draw_header(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default().borders(Borders::ALL);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(35), Constraint::Length(52)])
            .split(inner);

        let running = if self.state.running {
            "RUNNING"
        } else {
            "STOPPED"
        };
        let color = if self.state.running {
            Color::Green
        } else {
            Color::Red
        };

        let mut spans = vec![
            Span::styled(
                " MVX ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ModularVoiceX4yi "),
            Span::styled(
                format!(" {} ", running),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " {:.1}ms · {}",
                self.state.total_latency_ms, self.state.active_preset_name
            )),
        ];

        if self.state.bypass {
            spans.push(Span::styled(
                " [⚡BYPASS]",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
        if self.state.monitor_enabled {
            spans.push(Span::styled(
                " [🎧SIDETONE]",
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            ));
        }
        if self.state.muted {
            spans.push(Span::styled(
                " [🔇MUTE]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);

        let tabs = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(chunks[1]);
        self.draw_tab_button(frame, tabs[0], "1 Estudio", Tab::Estudio);
        self.draw_tab_button(frame, tabs[1], "2 Voice Lab", Tab::VoiceLab);
        self.draw_tab_button(frame, tabs[2], "3 Soundboard", Tab::Soundboard);
        self.draw_tab_button(frame, tabs[3], "4 Disp.", Tab::Dispositivos);
    }

    fn draw_tab_button(&mut self, frame: &mut Frame<'_>, area: Rect, label: &str, tab: Tab) {
        let selected = self.tab == tab;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {label} "), style)))
                .alignment(Alignment::Center),
            area,
        );
        self.register(area, Action::Tab(tab));
    }

    fn draw_estudio(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(26),
                Constraint::Percentage(44),
                Constraint::Percentage(30),
            ])
            .split(area);
        self.draw_presets(frame, chunks[0]);
        self.draw_estudio_center(frame, chunks[1]);
        self.draw_voice(frame, chunks[2]);
    }

    fn draw_presets(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Presets ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 4 {
            return;
        }
        let action_height = 2;
        let list_area = Rect {
            height: inner.height.saturating_sub(action_height + 1),
            ..inner
        };
        let presets = self.state.presets.clone();
        let capacity = list_area.height as usize;
        let selected_index = presets
            .iter()
            .position(|preset| preset.id == self.selected_preset)
            .unwrap_or(0);
        let start = selected_index.saturating_sub(capacity.saturating_sub(1));
        for (row, preset) in presets.iter().skip(start).take(capacity).enumerate() {
            let y = list_area.y + row as u16;
            let rect = Rect::new(list_area.x, y, list_area.width, 1);
            let selected = preset.id == self.selected_preset;
            let active = preset.id == self.state.active_preset;
            let marker = if active { "●" } else { " " };
            let kind = if preset.builtin { "" } else { " · personal" };
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if active {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(preset.name.clone(), style),
                    Span::styled(
                        kind,
                        if selected {
                            style
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    ),
                ])),
                rect,
            );
            self.register(rect, Action::SelectPreset(preset.id.clone()));
        }

        let new_rect = Rect::new(
            inner.x,
            inner.y + inner.height - action_height,
            inner.width,
            1,
        );
        self.draw_button(frame, new_rect, "＋ Nuevo preset", Action::NewPreset, true);

        let actions = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(actions);
        let builtin = self
            .selected()
            .map(|preset| preset.builtin)
            .unwrap_or(false);
        self.draw_button(frame, halves[0], "Restaurar", Action::ResetPreset, builtin);
        self.draw_button(frame, halves[1], "Borrar", Action::DeletePreset, !builtin);
    }

    fn draw_estudio_center(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Controles de Estudio y Monitoreo ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 10 {
            return;
        }

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Meters
                Constraint::Length(1), // Spectrum
                Constraint::Length(1), // Master Volume
                Constraint::Length(1), // Preset Gain
                Constraint::Length(1), // Mute & Bypass
                Constraint::Length(1), // Sidetone
                Constraint::Length(1), // Sidetone gain
                Constraint::Length(1), // Loop Tester
                Constraint::Length(1), // Noise reduction
                Constraint::Length(1), // Noise strength
                Constraint::Length(1), // Speaker Lock
                Constraint::Length(2), // Speaker status
                Constraint::Length(1), // Speaker profile actions
                Constraint::Min(1),    // Latency info
            ])
            .split(inner);

        // Meter VU bar
        let in_ratio = db_to_ratio(self.state.input_peak_db);
        let out_ratio = db_to_ratio(self.state.output_peak_db);
        let meter_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        frame.render_widget(
            Gauge::default()
                .gauge_style(Style::default().fg(if self.state.input_peak_db > -3.0 {
                    Color::Red
                } else {
                    Color::Green
                }))
                .ratio(in_ratio)
                .label(format!("In: {:.1} dB", self.state.input_peak_db)),
            meter_cols[0],
        );
        frame.render_widget(
            Gauge::default()
                .gauge_style(Style::default().fg(if self.state.output_peak_db > -3.0 {
                    Color::Red
                } else {
                    Color::Cyan
                }))
                .ratio(out_ratio)
                .label(format!("Out: {:.1} dB", self.state.output_peak_db)),
            meter_cols[1],
        );

        // Real-time 8-band Spectrum Visualizer
        let mut spectrum_spans = vec![Span::styled(
            "Espectro: ",
            Style::default().fg(Color::DarkGray),
        )];
        let band_labels = ["60", "150", "400", "1k", "2.5k", "5k", "8k", "14k"];
        for (i, label) in band_labels.iter().enumerate() {
            let db = self.state.spectrum_db.get(i).copied().unwrap_or(-60.0);
            let bar = spark_bar(db);
            let bar_color = if db > -6.0 {
                Color::Red
            } else if db > -18.0 {
                Color::Yellow
            } else if db > -35.0 {
                Color::Cyan
            } else {
                Color::DarkGray
            };
            spectrum_spans.push(Span::raw(format!("{label}:")));
            spectrum_spans.push(Span::styled(
                format!("{bar} "),
                Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spectrum_spans)), rows[1]);

        let Some(preset) = self.selected_or_active().cloned() else {
            return;
        };

        // Master Volume
        self.draw_slider(
            frame,
            rows[2],
            "Volumen maestro",
            self.master_edit,
            -24.0,
            24.0,
            "dB",
            SliderKind::Master,
        );

        // Preset Gain
        let preset_gain = preset.gain_db();
        self.draw_slider(
            frame,
            rows[3],
            "Ganancia del preset",
            preset_gain,
            -24.0,
            24.0,
            "dB",
            SliderKind::PresetGain,
        );

        // Toggles: Mute & Bypass
        let mute_bypass = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[4]);
        let mute_label = if self.state.muted {
            "[x] Silenciar (M)"
        } else {
            "[ ] Silenciar (M)"
        };
        self.draw_toggle(
            frame,
            mute_bypass[0],
            mute_label,
            Action::ToggleMute,
            self.state.muted,
        );

        let bypass_label = if self.state.bypass {
            "[x] DSP Bypass (B)"
        } else {
            "[ ] DSP Bypass (B)"
        };
        self.draw_toggle(
            frame,
            mute_bypass[1],
            bypass_label,
            Action::ToggleBypass,
            self.state.bypass,
        );

        // Sidetone
        let sidetone_label = if self.state.monitor_enabled {
            "[x] Sidetone / Retorno auriculares (S)"
        } else {
            "[ ] Sidetone / Retorno auriculares (S)"
        };
        self.draw_toggle(
            frame,
            rows[5],
            sidetone_label,
            Action::ToggleMonitor,
            self.state.monitor_enabled,
        );

        self.draw_slider(
            frame,
            rows[6],
            "Volumen Sidetone",
            self.state.monitor_gain_db,
            -24.0,
            12.0,
            "dB",
            SliderKind::MonitorGain,
        );

        // Voice Tester Loop
        let tester_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(rows[7]);
        let loop_text = match self.state.loop_test_state.as_str() {
            "recording" => "● Grabando 8s...",
            "looping" => "▶ Reproduciendo bucle",
            _ => "Loop Tester: inactivo",
        };
        let loop_color = match self.state.loop_test_state.as_str() {
            "recording" => Color::Red,
            "looping" => Color::Green,
            _ => Color::Gray,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                loop_text,
                Style::default().fg(loop_color).add_modifier(Modifier::BOLD),
            ))),
            tester_cols[0],
        );
        let btn_label = if self.state.loop_test_state == "idle" {
            "● Grabar 8s (L)"
        } else {
            "■ Detener bucle (L)"
        };
        self.draw_button(
            frame,
            tester_cols[1],
            btn_label,
            Action::ToggleLoopRecord,
            true,
        );

        // RNNoise
        let nr = preset.noise_reduction();
        let nr_label = if nr.is_some() {
            "[x] Reducción de ruido (RNNoise)"
        } else {
            "[ ] Reducción de ruido (RNNoise)"
        };
        self.draw_toggle(frame, rows[8], nr_label, Action::ToggleNoise, nr.is_some());
        self.draw_slider(
            frame,
            rows[9],
            "Intensidad NR",
            nr.unwrap_or(0.0),
            0.0,
            1.0,
            "",
            SliderKind::Noise,
        );

        // Speaker Lock
        let speaker = preset.speaker_lock();
        let speaker_label = if speaker.is_some() {
            "[x] Solo mi voz (CAM++)"
        } else {
            "[ ] Solo mi voz (CAM++)"
        };
        self.draw_toggle(
            frame,
            rows[10],
            speaker_label,
            Action::ToggleSpeaker,
            speaker.is_some(),
        );

        if speaker.is_some() {
            self.draw_slider(
                frame,
                rows[11],
                "Umbral coincidencia",
                speaker.as_ref().map(|lock| lock.threshold).unwrap_or(0.72),
                0.50,
                0.99,
                "",
                SliderKind::Threshold,
            );
        } else {
            frame.render_widget(
                Paragraph::new(self.state.speaker_status.clone())
                    .style(Style::default().fg(Color::Gray))
                    .wrap(Wrap { trim: true }),
                rows[11],
            );
        }

        let profile_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(44),
                Constraint::Percentage(28),
                Constraint::Percentage(28),
            ])
            .split(rows[12]);
        let profile_label = format!(
            "Perfil: {}",
            speaker
                .as_ref()
                .map(|lock| {
                    if lock.profile.is_empty() {
                        "(elegir)"
                    } else {
                        &lock.profile
                    }
                })
                .unwrap_or("(ninguno)")
        );
        self.draw_button(
            frame,
            profile_row[0],
            &profile_label,
            Action::CycleProfile,
            true,
        );
        self.draw_button(
            frame,
            profile_row[1],
            "Enrolar",
            Action::EnrollVoice,
            true,
        );
        if !self.state.models_installed {
            self.draw_button(
                frame,
                profile_row[2],
                "Modelos",
                Action::InstallModels,
                true,
            );
        } else {
            let has_profile = speaker
                .as_ref()
                .is_some_and(|lock| !lock.profile.trim().is_empty());
            self.draw_button(
                frame,
                profile_row[2],
                "Borrar",
                Action::DeleteVoice,
                has_profile,
            );
        }

        frame.render_widget(
            Paragraph::new(vec![Line::from(format!(
                "Latencia: base {:.1} + DSP {:.1} = {:.1} ms · Quantum {} · Xruns {}",
                self.state.latency_ms,
                self.state.dsp_latency_ms,
                self.state.total_latency_ms,
                self.state.quantum,
                self.state.xruns
            ))])
            .style(Style::default().fg(Color::DarkGray)),
            rows[13],
        );
    }

    fn draw_voice_lab(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Voice Lab: {} ", self.state.active_preset_name));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 10 {
            return;
        }

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);

        let Some(preset) = self.selected_or_active().cloned() else {
            return;
        };

        // LEFT COLUMN: Pre-procesamiento y Dinámica (HPF, Gate, EQ, Compressor)
        let left_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // HPF toggle
                Constraint::Length(1), // HPF Cutoff
                Constraint::Length(1), // Gate toggle
                Constraint::Length(1), // Gate threshold
                Constraint::Length(1), // Gate reduction status
                Constraint::Length(1), // EQ toggle
                Constraint::Length(1), // EQ Low
                Constraint::Length(1), // EQ Mid
                Constraint::Length(1), // EQ High
                Constraint::Length(1), // Compressor toggle
                Constraint::Length(1), // Comp threshold
                Constraint::Length(1), // Comp ratio
                Constraint::Length(1), // Comp makeup
                Constraint::Min(1),    // Comp reduction status
            ])
            .split(cols[0]);

        // HPF
        let hpf = preset.high_pass();
        self.draw_toggle(
            frame,
            left_rows[0],
            if hpf.is_some() {
                "[x] Filtro Paso Alto (HPF)"
            } else {
                "[ ] Filtro Paso Alto (HPF)"
            },
            Action::ToggleFilter("high_pass"),
            hpf.is_some(),
        );
        self.draw_slider(
            frame,
            left_rows[1],
            "Corte HPF",
            hpf.unwrap_or(80.0),
            20.0,
            300.0,
            "Hz",
            SliderKind::HpfCutoff,
        );

        // Noise Gate
        let gate = preset.noise_gate();
        self.draw_toggle(
            frame,
            left_rows[2],
            if gate.is_some() {
                "[x] Puerta de Ruido (Noise Gate)"
            } else {
                "[ ] Puerta de Ruido (Noise Gate)"
            },
            Action::ToggleFilter("noise_gate"),
            gate.is_some(),
        );
        self.draw_slider(
            frame,
            left_rows[3],
            "Umbral Gate",
            gate.as_ref().map(|g| g.threshold_db).unwrap_or(-45.0),
            -60.0,
            0.0,
            "dB",
            SliderKind::GateThreshold,
        );
        frame.render_widget(
            Paragraph::new(format!(
                "  Atenuación Gate: -{:.1} dB",
                self.state.gate_reduction_db
            ))
            .style(Style::default().fg(Color::DarkGray)),
            left_rows[4],
        );

        // 3-Band Parametric EQ
        let eq = preset.equalizer();
        self.draw_toggle(
            frame,
            left_rows[5],
            if eq.is_some() {
                "[x] Ecualizador 3-Bandas"
            } else {
                "[ ] Ecualizador 3-Bandas"
            },
            Action::ToggleFilter("equalizer"),
            eq.is_some(),
        );
        let (low, mid, high) = eq.unwrap_or((0.0, 0.0, 0.0));
        self.draw_slider(
            frame,
            left_rows[6],
            "EQ Graves (100Hz)",
            low,
            -15.0,
            15.0,
            "dB",
            SliderKind::EqLow,
        );
        self.draw_slider(
            frame,
            left_rows[7],
            "EQ Medios (1kHz)",
            mid,
            -15.0,
            15.0,
            "dB",
            SliderKind::EqMid,
        );
        self.draw_slider(
            frame,
            left_rows[8],
            "EQ Agudos (8kHz)",
            high,
            -15.0,
            15.0,
            "dB",
            SliderKind::EqHigh,
        );

        // Compressor
        let comp = preset.compressor();
        self.draw_toggle(
            frame,
            left_rows[9],
            if comp.is_some() {
                "[x] Compresor de Estudio"
            } else {
                "[ ] Compresor de Estudio"
            },
            Action::ToggleFilter("compressor"),
            comp.is_some(),
        );
        self.draw_slider(
            frame,
            left_rows[10],
            "Umbral Compresor",
            comp.as_ref().map(|c| c.threshold_db).unwrap_or(-18.0),
            -40.0,
            0.0,
            "dB",
            SliderKind::CompThreshold,
        );
        self.draw_slider(
            frame,
            left_rows[11],
            "Ratio Compresor",
            comp.as_ref().map(|c| c.ratio).unwrap_or(3.0),
            1.0,
            10.0,
            ":1",
            SliderKind::CompRatio,
        );
        self.draw_slider(
            frame,
            left_rows[12],
            "Ganancia Makeup",
            comp.as_ref().map(|c| c.makeup_db).unwrap_or(2.0),
            0.0,
            24.0,
            "dB",
            SliderKind::CompMakeup,
        );
        frame.render_widget(
            Paragraph::new(format!(
                "  Reducción Compresor: -{:.1} dB",
                self.state.comp_reduction_db
            ))
            .style(Style::default().fg(Color::DarkGray)),
            left_rows[13],
        );

        // RIGHT COLUMN: Efectos de Carácter y Modulación (Robot, RingMod, Dist, Chorus, Reverb)
        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Robotizer toggle
                Constraint::Length(1), // Robotizer freq
                Constraint::Length(1), // Robotizer mix
                Constraint::Length(1), // RingMod toggle
                Constraint::Length(1), // RingMod freq
                Constraint::Length(1), // RingMod mix
                Constraint::Length(1), // Distortion toggle
                Constraint::Length(1), // Distortion drive
                Constraint::Length(1), // Distortion mix
                Constraint::Length(1), // Chorus toggle
                Constraint::Length(1), // Chorus mix
                Constraint::Length(1), // Reverb toggle
                Constraint::Length(1), // Reverb room
                Constraint::Min(1),    // Reverb mix
            ])
            .split(cols[1]);

        // Robotizer
        let robot = preset.robotizer();
        self.draw_toggle(
            frame,
            right_rows[0],
            if robot.is_some() {
                "[x] Robotizador"
            } else {
                "[ ] Robotizador"
            },
            Action::ToggleFilter("robotizer"),
            robot.is_some(),
        );
        let (rfreq, rmix) = robot.unwrap_or((80.0, 0.5));
        self.draw_slider(
            frame,
            right_rows[1],
            "Frecuencia Robot",
            rfreq,
            20.0,
            500.0,
            "Hz",
            SliderKind::RobotFreq,
        );
        self.draw_slider(
            frame,
            right_rows[2],
            "Mezcla Robot",
            rmix,
            0.0,
            1.0,
            "",
            SliderKind::RobotMix,
        );

        // Ring Modulator
        let ring = preset.ring_mod();
        self.draw_toggle(
            frame,
            right_rows[3],
            if ring.is_some() {
                "[x] Modulador en Anillo (Alien)"
            } else {
                "[ ] Modulador en Anillo (Alien)"
            },
            Action::ToggleFilter("ring_mod"),
            ring.is_some(),
        );
        let (ring_f, ring_m) = ring.unwrap_or((240.0, 0.5));
        self.draw_slider(
            frame,
            right_rows[4],
            "Frecuencia Portadora",
            ring_f,
            50.0,
            2000.0,
            "Hz",
            SliderKind::RingFreq,
        );
        self.draw_slider(
            frame,
            right_rows[5],
            "Mezcla Anillo",
            ring_m,
            0.0,
            1.0,
            "",
            SliderKind::RingMix,
        );

        // Distortion
        let dist = preset.distortion();
        self.draw_toggle(
            frame,
            right_rows[6],
            if dist.is_some() {
                "[x] Distorsión / Saturación"
            } else {
                "[ ] Distorsión / Saturación"
            },
            Action::ToggleFilter("distortion"),
            dist.is_some(),
        );
        self.draw_slider(
            frame,
            right_rows[7],
            "Drive Distorsión",
            dist.as_ref().map(|d| d.drive).unwrap_or(6.0),
            0.0,
            24.0,
            "dB",
            SliderKind::DistDrive,
        );
        self.draw_slider(
            frame,
            right_rows[8],
            "Mezcla Distorsión",
            dist.as_ref().map(|d| d.mix).unwrap_or(0.5),
            0.0,
            1.0,
            "",
            SliderKind::DistMix,
        );

        // Chorus
        let chorus = preset.chorus();
        self.draw_toggle(
            frame,
            right_rows[9],
            if chorus.is_some() {
                "[x] Chorus / Doubler"
            } else {
                "[ ] Chorus / Doubler"
            },
            Action::ToggleFilter("chorus"),
            chorus.is_some(),
        );
        self.draw_slider(
            frame,
            right_rows[10],
            "Mezcla Chorus",
            chorus.as_ref().map(|c| c.mix).unwrap_or(0.4),
            0.0,
            1.0,
            "",
            SliderKind::ChorusMix,
        );

        // Reverb
        let reverb = preset.reverb();
        self.draw_toggle(
            frame,
            right_rows[11],
            if reverb.is_some() {
                "[x] Reverberación de Sala"
            } else {
                "[ ] Reverberación de Sala"
            },
            Action::ToggleFilter("reverb"),
            reverb.is_some(),
        );
        self.draw_slider(
            frame,
            right_rows[12],
            "Tamaño de Sala",
            reverb.as_ref().map(|r| r.room_size).unwrap_or(0.6),
            0.0,
            1.0,
            "",
            SliderKind::ReverbRoom,
        );
        self.draw_slider(
            frame,
            right_rows[13],
            "Mezcla Reverb",
            reverb.as_ref().map(|r| r.mix).unwrap_or(0.3),
            0.0,
            1.0,
            "",
            SliderKind::ReverbMix,
        );
    }

    fn draw_soundboard(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Tablero de Sonidos (Soundboard) ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 6 {
            return;
        }

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Active playback status
                Constraint::Length(1), // Stop button
                Constraint::Min(4),    // Sound list / grid
                Constraint::Length(3), // Footer hints
            ])
            .split(inner);

        // Active playback banner
        let (status_text, status_color) = if let Some(active) = &self.state.active_sound_clip {
            (
                format!("▶ Reproduciendo: «{}» (Micrófono atenuado automáticamente -70%)", active),
                Color::Green,
            )
        } else {
            (
                "■ En espera (Micrófono al 100%)".to_string(),
                Color::DarkGray,
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                status_text,
                Style::default().fg(status_color).add_modifier(Modifier::BOLD),
            ))),
            rows[0],
        );

        // Stop all button
        self.draw_button(
            frame,
            rows[1],
            "■ Detener todos los sonidos (Espacio)",
            Action::StopSounds,
            true,
        );

        // Sound clips list
        let clips = self.state.sound_clips.clone();
        let capacity = rows[2].height as usize;
        let start = self.selected_sound.saturating_sub(capacity.saturating_sub(1));
        for (row, (index, clip)) in clips
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity)
            .enumerate()
        {
            let rect = Rect::new(rows[2].x, rows[2].y + row as u16, rows[2].width, 1);
            let selected = index == self.selected_sound;
            let is_playing = self.state.active_sound_clip.as_deref() == Some(&clip.id);
            let hotkey = if index < 9 {
                format!("[{}]", index + 1)
            } else {
                "   ".to_string()
            };
            let play_icon = if is_playing { "▶ " } else { "  " };

            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_playing {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let label = format!(
                "{} {} {:<24} ({:.1}s) {}",
                hotkey,
                play_icon,
                clip.name,
                clip.duration_secs,
                if clip.builtin { "· built-in" } else { "· audio" }
            );

            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(label, style))),
                rect,
            );
            self.register(rect, Action::TriggerSound(clip.id.clone()));
        }

        // Footer hints
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("💡 Directorio de clips: ~/.local/share/mvx/sounds/ (archivos WAV estándar)"),
                Line::from("   Atajos globales: 'mvx ctl sound <id>' para teclas rápidas en Hyprland / Sway / KDE"),
            ])
            .style(Style::default().fg(Color::DarkGray)),
            rows[3],
        );
    }

    fn draw_voice(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Modulación de voz ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 8 {
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);
        let Some(preset) = self.selected_or_active().cloned() else {
            return;
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        preset.name.clone(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(if preset.builtin {
                        "  · built-in"
                    } else {
                        "  · personal"
                    }),
                ]),
                Line::from(format!(
                    "Pitch {:+.1} st · Formante {:+.1} st",
                    preset.pitch_semitones, preset.formant_semitones
                )),
            ]),
            rows[0],
        );
        self.draw_slider(
            frame,
            rows[1],
            "Tono (pitch)",
            preset.pitch_semitones,
            -12.0,
            12.0,
            "st",
            SliderKind::Pitch,
        );
        self.draw_slider(
            frame,
            rows[2],
            "Formantes",
            preset.formant_semitones,
            -6.0,
            6.0,
            "st",
            SliderKind::Formant,
        );
        frame.render_widget(
            Paragraph::new("Calidad DSP (latencia / CPU)").style(Style::default().fg(Color::Gray)),
            rows[3],
        );
        let qualities = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(34),
                Constraint::Percentage(33),
            ])
            .split(rows[4]);
        for (index, quality) in [Quality::Ultra, Quality::Balanced, Quality::High]
            .into_iter()
            .enumerate()
        {
            let short = match quality {
                Quality::Ultra => "Ultra",
                Quality::Balanced => "Equil.",
                Quality::High => "Alta",
            };
            let label = format!("{short} {:.0}ms", quality.latency_ms());
            let selected = preset.quality == quality;
            let rect = qualities[index];
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    if selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                )))
                .alignment(Alignment::Center),
                rect,
            );
            self.register(rect, Action::Quality(quality));
        }
        let actions = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[5]);
        self.draw_button(
            frame,
            actions[0],
            "Aplicar preset",
            Action::ApplyPreset,
            true,
        );
        self.draw_button(
            frame,
            actions[1],
            "Restaurar",
            Action::ResetPreset,
            preset.builtin,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from("El pitch altera el tono fundamental."),
                Line::from("Los formantes cambian el timbre de garganta."),
                Line::from(""),
                Line::from("0 st en ambos = voz natural."),
            ])
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true }),
            rows[6],
        );
    }

    fn draw_devices(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Entrada física ");
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let capacity = inner.height.saturating_sub(2) as usize;
        let start = self
            .selected_device
            .saturating_sub(capacity.saturating_sub(1));
        let devices = self.state.devices.clone();
        for (row, (index, device)) in devices
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity)
            .enumerate()
        {
            let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
            let selected = self.state.selected_input.as_deref() == Some(device.name.as_str());
            let highlighted = index == self.selected_device;
            let style = if highlighted {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(if selected { "● " } else { "  " }),
                    Span::styled(device.description.clone(), style),
                ])),
                rect,
            );
            self.register(rect, Action::SelectDevice(index));
        }
        let button = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        self.draw_button(
            frame,
            button,
            "Actualizar dispositivos",
            Action::Rescan,
            true,
        );
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Min(1),
            ])
            .split(chunks[1]);
        let input = self
            .state
            .selected_input_description
            .clone()
            .or_else(|| self.state.selected_input.clone())
            .unwrap_or_else(|| "(sin entrada)".into());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Entrada: ", Style::default().fg(Color::Gray)),
                    Span::raw(input),
                ]),
                Line::from(format!("Salida: {} (virtual)", self.state.virtual_source)),
                Line::from(format!(
                    "Formato: {} Hz · {} ch",
                    self.state.sample_rate, self.state.channels
                )),
                Line::from(format!(
                    "Quantum: {} · latencia base {:.2} ms",
                    self.state.quantum, self.state.latency_ms
                )),
                Line::from(format!(
                    "DSP: {:.2} ms · total {:.2} ms",
                    self.state.dsp_latency_ms, self.state.total_latency_ms
                )),
            ])
            .block(Block::default().borders(Borders::ALL).title(" Salida ")),
            right[0],
        );
        let in_ratio = db_to_ratio(self.state.input_peak_db);
        let out_ratio = db_to_ratio(self.state.output_peak_db);
        let gauges = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(3)])
            .split(right[1]);
        frame.render_widget(
            Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Peak entrada "),
                )
                .gauge_style(Style::default().fg(if self.state.input_peak_db > -3.0 {
                    Color::Red
                } else {
                    Color::Green
                }))
                .ratio(in_ratio)
                .label(format!("{:.1} dB", self.state.input_peak_db)),
            gauges[0],
        );
        frame.render_widget(
            Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Peak salida "),
                )
                .gauge_style(Style::default().fg(if self.state.output_peak_db > -3.0 {
                    Color::Red
                } else {
                    Color::Cyan
                }))
                .ratio(out_ratio)
                .label(format!("{:.1} dB", self.state.output_peak_db)),
            gauges[1],
        );
        let error = self
            .state
            .error
            .clone()
            .map(|e| format!("ERROR: {e}"))
            .unwrap_or_else(|| "Sin errores".into());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "Xruns {} · underruns {} · overflows {}",
                    self.state.xruns, self.state.underruns, self.state.overflows
                )),
                Line::from(format!(
                    "Bloques captura {} · playback {}",
                    self.state.capture_blocks, self.state.playback_blocks
                )),
                Line::from(""),
                Line::from(error),
                Line::from(self.status.clone()),
            ])
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Diagnóstico "),
            ),
            right[2],
        );
    }

    fn draw_footer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let help = Line::from(vec![
            Span::styled(" q/Esc ", key_style()),
            Span::raw("salir "),
            Span::styled(" 1-4/Tab ", key_style()),
            Span::raw("pestañas "),
            Span::styled(" m ", key_style()),
            Span::raw("mute "),
            Span::styled(" b ", key_style()),
            Span::raw("bypass "),
            Span::styled(" s ", key_style()),
            Span::raw("sidetone "),
            Span::styled(" v ", key_style()),
            Span::raw("mi voz "),
            Span::styled(" l ", key_style()),
            Span::raw("loop "),
            Span::styled(" Espacio ", key_style()),
            Span::raw("parar fx "),
            Span::styled(" +/- ", key_style()),
            Span::raw("volumen"),
        ]);
        let status = Line::from(vec![
            Span::styled(" estado: ", Style::default().fg(Color::Gray)),
            Span::raw(self.status.clone()),
        ]);
        frame.render_widget(Paragraph::new(vec![help, status]), area);
    }

    fn draw_modal(&mut self, frame: &mut Frame<'_>) {
        let Some(modal) = self.modal.clone() else {
            return;
        };
        let area = frame.area();
        let width = area.width.saturating_sub(10).clamp(30, 64);
        let height = 9;
        let rect = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", modal.title()))
            .style(Style::default().bg(Color::Black));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);
        frame.render_widget(
            Paragraph::new(modal.hint())
                .style(Style::default().fg(Color::Gray))
                .wrap(Wrap { trim: true }),
            lines[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan)),
                Span::raw(modal.value().to_string()),
                Span::styled("▏", Style::default().fg(Color::Cyan)),
            ]))
            .style(Style::default().fg(Color::White)),
            lines[2],
        );
        let buttons = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(lines[4]);
        self.draw_button(frame, buttons[0], "Cancelar", Action::ModalCancel, true);
        self.draw_button(frame, buttons[1], "Confirmar", Action::ModalSubmit, true);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_slider(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        value: f32,
        min: f32,
        max: f32,
        unit: &str,
        kind: SliderKind,
    ) {
        let ratio = ((value - min) / (max - min)).clamp(0.0, 1.0) as f64;
        let label = match unit {
            "dB" | "st" => format!("{label}: {value:+.1} {unit}"),
            "Hz" | "ms" => format!("{label}: {value:.0} {unit}"),
            ":1" => format!("{label}: {value:.1}:1"),
            _ => format!("{label}: {value:.2}"),
        };
        frame.render_widget(
            Gauge::default().ratio(ratio).label(label).gauge_style(
                Style::default()
                    .fg(Color::Cyan)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
        );
        self.register(area, Action::Slider(kind));
    }

    fn draw_toggle(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        action: Action,
        active: bool,
    ) {
        let hovered = self.hover.is_some_and(|rect| rect == area);
        let style = if hovered {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {label} "), style)))
                .alignment(Alignment::Center),
            area,
        );
        self.register(area, action);
    }

    fn draw_button(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        action: Action,
        enabled: bool,
    ) {
        let hovered = self.hover.is_some_and(|rect| rect == area);
        let style = if !enabled {
            Style::default().fg(Color::DarkGray)
        } else if hovered {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {label} "), style)))
                .alignment(Alignment::Center),
            area,
        );
        if enabled {
            self.register(area, action);
        }
    }
}

fn key_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD)
}

fn db_to_ratio(db: f32) -> f64 {
    (((db + 60.0) / 60.0).clamp(0.0, 1.0)) as f64
}

fn spark_bar(db: f32) -> &'static str {
    if db < -55.0 {
        " "
    } else if db < -45.0 {
        "▂"
    } else if db < -35.0 {
        "▃"
    } else if db < -25.0 {
        "▄"
    } else if db < -18.0 {
        "▅"
    } else if db < -12.0 {
        "▆"
    } else if db < -6.0 {
        "▇"
    } else {
        "█"
    }
}

fn models_command(action: ModelsAction) -> Result<()> {
    match action {
        ModelsAction::Install => {
            let mut client = connect_or_start()?;
            match client.request(&Request::InstallModels)? {
                Response::Error { message } => println!("error: {message}"),
                _ => println!(
                    "descarga iniciada; modelos en {}",
                    mvx_core::models_dir().display()
                ),
            }
            println!("podés seguir el estado con `mvx status` o en la TUI.");
            Ok(())
        }
        ModelsAction::Status => {
            let speaker = mvx_core::speaker_model_path();
            let vad = mvx_core::vad_model_path();
            println!(
                "[{}] speaker: {}",
                if speaker.exists() { "ok" } else { "!!" },
                speaker.display()
            );
            println!(
                "[{}] VAD:     {}",
                if vad.exists() { "ok" } else { "!!" },
                vad.display()
            );
            println!("perfiles: {}", mvx_core::list_voice_profiles().join(", "));
            Ok(())
        }
    }
}

fn doctor() -> Result<()> {
    let path = socket_path();
    println!("MVX doctor");
    println!("  socket: {}", path.display());
    println!("  config: {}", mvx_core::config_path().display());
    println!("  presets: {}", mvx_core::presets_path().display());
    println!("  modelos: {}", mvx_core::models_dir().display());
    match Client::connect(&path) {
        Ok(mut client) => {
            println!("  [ok] daemon conectado");
            let state = client.state()?;
            println!(
                "  [{}] motor: {}",
                if state.running { "ok" } else { "!!" },
                if state.running { "running" } else { "stopped" }
            );
            println!("  [ok] fuente virtual: {}", state.virtual_source);
            println!(
                "  [{}] entrada: {}",
                if state.selected_input.is_some() {
                    "ok"
                } else {
                    "!!"
                },
                state
                    .selected_input_description
                    .clone()
                    .or(state.selected_input.clone())
                    .unwrap_or_else(|| "(ninguna)".into())
            );
            println!(
                "  [ok] formato: {} Hz / {} ch",
                state.sample_rate, state.channels
            );
            println!(
                "  [ok] quantum: {} · base {:.2} ms · DSP {:.2} ms · total {:.2} ms",
                state.quantum, state.latency_ms, state.dsp_latency_ms, state.total_latency_ms
            );
            println!(
                "  [ok] preset: {} ({})",
                state.active_preset_name, state.active_preset
            );
            println!(
                "  [{}] modelos de voz: {}",
                if state.models_installed { "ok" } else { "!!" },
                if state.models_installed {
                    "instalados"
                } else {
                    "faltan; ejecutá `mvx models install`"
                }
            );
            println!(
                "  [{}] perfiles de voz: {}",
                if state.voice_profiles.is_empty() {
                    "--"
                } else {
                    "ok"
                },
                if state.voice_profiles.is_empty() {
                    "sin perfiles (opcional)".to_string()
                } else {
                    state.voice_profiles.join(", ")
                }
            );
            println!(
                "  [{}] xruns: {} · underruns: {} · overflows: {}",
                if state.xruns == 0 { "ok" } else { "!!" },
                state.xruns,
                state.underruns,
                state.overflows
            );
            if let Some(err) = state.error {
                println!("  [!!] error: {err}");
            }
            Ok(())
        }
        Err(err) => {
            println!("  [!!] daemon no disponible: {err}");
            println!("       iniciá `mvxd` o `systemctl --user start mvxd`");
            Ok(())
        }
    }
}
