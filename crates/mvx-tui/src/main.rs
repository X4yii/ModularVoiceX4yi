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

use mvx_core::clone::{CloneStatus, VoiceCloneSession};
use mvx_core::presets::FilterConfig;
use mvx_core::{socket_path, ConfigResetTarget, Preset, Quality, Snapshot};
use mvx_ipc::{Client, Request, Response};

#[derive(Debug, Parser)]
#[command(name = "mvx", about = "MVX voice modulator client")]
struct Args {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Abrir la TUI interactiva.
    Tui,
    /// Imprimir snapshot del estado (usar --waybar para módulo Waybar JSON).
    Status {
        /// Formato JSON de una línea para barra de estado Waybar / Polybar.
        #[arg(long)]
        waybar: bool,
    },
    /// Comandos rápidos de control CLI (mute, bypass, monitor, presets, soundboard).
    Ctl {
        #[command(subcommand)]
        action: CtlAction,
    },
    /// Listar dispositivos de entrada físicos disponibles.
    Devices,
    /// Diagnosticar el daemon y la ruta de audio PipeWire.
    Doctor,
    /// Administrar modelos neuronales (CAM++ y Silero VAD).
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Solicitar a mvxd que se apague de forma limpia.
    Shutdown,
}

#[derive(Debug, Subcommand)]
enum CtlAction {
    /// Conmutar silenciado del micrófono.
    ToggleMute,
    /// Conmutar bypass del DSP (micrófono limpio).
    ToggleBypass,
    /// Conmutar Sidetone / retorno en auriculares en tiempo real.
    ToggleMonitor,
    /// Ajustar ganancia del retorno Sidetone (en dB).
    MonitorGain { gain_db: f32 },
    /// Aplicar un preset de voz por su ID.
    Preset { id: String },
    /// Pasar al siguiente preset.
    NextPreset,
    /// Pasar al preset anterior.
    PrevPreset,
    /// Reproducir clip de audio por su ID.
    Sound { id: String },
    /// Detener reproducción de clips.
    StopSounds,
    /// Controlar el Loop Tester (record, stop, toggle).
    LoopTest {
        #[arg(default_value = "toggle")]
        action: String,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsAction {
    /// Descargar modelo CAM++ y Silero VAD.
    Install,
    /// Imprimir rutas y estado de verificación de modelos.
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
                    println!(
                        "Sidetone / Retorno: {}",
                        if !state.monitor_enabled { "activado" } else { "desactivado" }
                    );
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
                        let cur_idx = state
                            .presets
                            .iter()
                            .position(|p| p.id == state.active_preset)
                            .unwrap_or(0);
                        let next_idx = (cur_idx + 1) % state.presets.len();
                        let next_id = &state.presets[next_idx].id;
                        client.apply_preset(next_id.clone())?;
                        println!("Preset: {}", state.presets[next_idx].name);
                    }
                }
                CtlAction::PrevPreset => {
                    let state = client.state()?;
                    if !state.presets.is_empty() {
                        let cur_idx = state
                            .presets
                            .iter()
                            .position(|p| p.id == state.active_preset)
                            .unwrap_or(0);
                        let prev_idx = if cur_idx == 0 {
                            state.presets.len() - 1
                        } else {
                            cur_idx - 1
                        };
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
            app.status = "Ratón listo: clic, arrastre o rueda para interactuar".into();
        }
        Err(err) => {
            app.mouse_active = false;
            app.status = format!("Ratón no disponible: {err}; navegá con teclado");
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
    Perfiles,
    Filtros,
    Dispositivos,
    Clonacion,
    Ajustes,
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
    ClonePitch,
    CloneFormant,
    CloneBlend,
    CloneDenoise,
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
    // Reset Target
    OpenResetConfirm(ConfigResetTarget),
    // Voice Clone actions
    OpenCloneFileInput,
    StartCloneRecording,
    TestCloneVoice,
    OpenCloneSaveModal,
    DiscardCloneSession,
    // Modals
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
    ConfirmReset { target: ConfigResetTarget },
    CloneFileInput { path: String },
    CloneSaveName { name: String },
}

impl Modal {
    fn value(&self) -> &str {
        match self {
            Self::NewPreset { value } | Self::EnrollVoice { value } => value,
            Self::CloneFileInput { path } => path,
            Self::CloneSaveName { name } => name,
            Self::ConfirmReset { .. } => "",
        }
    }

    fn value_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::NewPreset { value } | Self::EnrollVoice { value } => Some(value),
            Self::CloneFileInput { path } => Some(path),
            Self::CloneSaveName { name } => Some(name),
            Self::ConfirmReset { .. } => None,
        }
    }

    fn title(&self) -> &'static str {
        match self {
            Self::NewPreset { .. } => "Crear nuevo perfil de voz",
            Self::EnrollVoice { .. } => "Enrolar mi voz (CAM++)",
            Self::ConfirmReset { .. } => "Confirmación de restablecimiento",
            Self::CloneFileInput { .. } => "Cargar muestra de voz para clonar",
            Self::CloneSaveName { .. } => "Guardar voz clonada como perfil",
        }
    }

    fn hint(&self) -> &'static str {
        match self {
            Self::NewPreset { .. } => {
                "Ingresá el nombre del perfil. Se creará una copia editable de la configuración actual."
            }
            Self::EnrollVoice { .. } => {
                "Nombre del perfil de voz. Al confirmar, hablá 10s al micrófono para extraer el embedding."
            }
            Self::ConfirmReset { target } => match target {
                ConfigResetTarget::All => {
                    "¿Restablecer TODA la configuración de fábrica? (Audio, Presets y DSP)"
                }
                ConfigResetTarget::Audio => {
                    "¿Restablecer la configuración de audio y conexión PipeWire por defecto?"
                }
                ConfigResetTarget::Presets => {
                    "¿Restablecer todos los presets a los valores originales de fábrica?"
                }
                ConfigResetTarget::Dsp => {
                    "¿Restablecer la cadena de efectos y filtros DSP por defecto?"
                }
            },
            Self::CloneFileInput { .. } => {
                "Escribí la ruta absoluta o relativa del archivo de voz (WAV, MP3, OGG o FLAC)."
            }
            Self::CloneSaveName { .. } => {
                "Ingresá el nombre con el que se guardará el nuevo preset en tu biblioteca."
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
    clone_session: VoiceCloneSession,
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
            tab: Tab::Perfiles,
            selected_device: 0,
            status: "Conectando al daemon PipeWire…".into(),
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
            clone_session: VoiceCloneSession::default(),
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
                self.status = format!("Ratón no disponible: {err}; usá el teclado");
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
                    self.clone_session = state.clone_session.clone();
                    self.state = state;
                }
                self.status = "Conectado a mvxd (PipeWire nativo)".into();
                self.client = Some(client);
                self.ensure_tray();
            }
            Err(err) => {
                self.status = format!("Esperando daemon: {err}");
            }
        }
    }

    fn ensure_tray(&mut self) {
        if self.tray_checked || self.client.is_none() {
            return;
        }
        self.tray_checked = true;
        if let Err(err) = start_tray() {
            self.status = format!("Tray no iniciado: {err}");
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
                self.status = format!("Daemon desconectado: {err}");
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
            self.status = "Sin daemon".into();
            return;
        };
        match client.request(&request) {
            Ok(Response::Error { message }) => self.status = message,
            Ok(_) => {}
            Err(err) => {
                self.client = None;
                self.status = format!("Error IPC: {err}");
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
                    Tab::Perfiles => Tab::Filtros,
                    Tab::Filtros => Tab::Dispositivos,
                    Tab::Dispositivos => Tab::Clonacion,
                    Tab::Clonacion => Tab::Ajustes,
                    Tab::Ajustes => Tab::Perfiles,
                };
            }
            KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Perfiles => Tab::Ajustes,
                    Tab::Filtros => Tab::Perfiles,
                    Tab::Dispositivos => Tab::Filtros,
                    Tab::Clonacion => Tab::Dispositivos,
                    Tab::Ajustes => Tab::Clonacion,
                };
            }
            KeyCode::F(1) | KeyCode::Char('1') => self.tab = Tab::Perfiles,
            KeyCode::F(2) | KeyCode::Char('2') => self.tab = Tab::Filtros,
            KeyCode::F(3) | KeyCode::Char('3') => self.tab = Tab::Dispositivos,
            KeyCode::F(4) | KeyCode::Char('4') => self.tab = Tab::Clonacion,
            KeyCode::F(5) | KeyCode::Char('5') => self.tab = Tab::Ajustes,

            KeyCode::Up | KeyCode::Char('k') => match self.tab {
                Tab::Perfiles | Tab::Filtros => self.move_preset(-1),
                Tab::Dispositivos => {
                    self.selected_device = self.selected_device.saturating_sub(1);
                }
                _ => {}
            },
            KeyCode::Down | KeyCode::Char('j') => match self.tab {
                Tab::Perfiles | Tab::Filtros => self.move_preset(1),
                Tab::Dispositivos => {
                    if !self.state.devices.is_empty() {
                        self.selected_device =
                            (self.selected_device + 1).min(self.state.devices.len() - 1);
                    }
                }
                _ => {}
            },
            KeyCode::Enter => match self.tab {
                Tab::Perfiles | Tab::Filtros => self.apply_selected(),
                Tab::Dispositivos => self.select_device(self.selected_device),
                _ => {}
            },
            KeyCode::Char('s') => self.toggle_monitor(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('b') => self.toggle_bypass(),
            KeyCode::Char('l') => self.toggle_loop_test(),
            KeyCode::Char('n') => self.open_new_preset(),
            KeyCode::Char('r') => self.reset_selected(),
            KeyCode::Char('d') => self.delete_selected(),
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
                    if let Some(val) = modal.value_mut() {
                        val.pop();
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(modal) = self.modal.as_mut() {
                    if let Some(val) = modal.value_mut() {
                        if val.len() < 128 {
                            val.push(c);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn submit_modal(&mut self) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal {
            Modal::NewPreset { value } => {
                let val = value.trim();
                if val.is_empty() {
                    self.status = "El nombre no puede estar vacío".into();
                    return;
                }
                let base = self
                    .selected_or_active()
                    .cloned()
                    .unwrap_or_else(|| self.state.presets[0].clone());
                let id = self.next_custom_id();
                let mut preset = base;
                preset.id = id;
                preset.name = val.to_string();
                preset.builtin = false;
                self.selected_preset = preset.id.clone();
                self.draft = Some(preset.clone());
                self.needs_send = false;
                self.send(Request::UpsertPreset {
                    preset,
                    activate: true,
                });
                self.status = format!("Perfil creado: {val}");
            }
            Modal::EnrollVoice { value } => {
                let val = value.trim();
                if val.is_empty() {
                    self.status = "El nombre no puede estar vacío".into();
                    return;
                }
                if let Err(err) = mvx_core::validate_profile_name(val) {
                    self.status = format!("Nombre de perfil inválido: {err}");
                    return;
                }
                let Some(mut preset) = self.selected_or_active().cloned() else {
                    self.status = "No hay un preset activo".into();
                    return;
                };
                if !preset.has_filter("speaker_lock") {
                    preset.add_filter("speaker_lock");
                }
                preset.filters.iter_mut().for_each(|filter| {
                    if let FilterConfig::SpeakerLock {
                        enabled, profile, ..
                    } = filter
                    {
                        *enabled = true;
                        *profile = val.to_string();
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
                    name: val.to_string(),
                    seconds: 10,
                });
                self.status = format!("Enrolamiento de voz iniciado (10s): {val}");
            }
            Modal::ConfirmReset { target } => {
                self.execute_reset(target);
            }
            Modal::CloneFileInput { path } => {
                let p = PathBuf::from(path.trim());
                if let Err(err) = self.clone_session.validate_source(&p) {
                    self.status = format!("Error al cargar audio: {err}");
                } else {
                    self.status = format!("Muestra lista: {}", p.display());
                    if let Some(client) = self.client.as_mut() {
                        let _ = client.clone_voice_validate(p.to_string_lossy().to_string());
                    }
                }
            }
            Modal::CloneSaveName { name } => {
                let val = name.trim();
                if val.is_empty() {
                    self.status = "El nombre no puede estar vacío".into();
                    return;
                }
                self.clone_session.name = val.to_string();
                let preset = self.clone_session.to_preset();
                let session = self.clone_session.clone();
                self.selected_preset = preset.id.clone();
                self.draft = Some(preset.clone());
                self.send(Request::UpsertPreset {
                    preset,
                    activate: true,
                });
                if let Some(client) = self.client.as_mut() {
                    let _ = client.clone_voice_save(session);
                }
                self.status = format!("Voz clonada guardada como perfil: {val}");
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
                    } else if matches!(self.tab, Tab::Perfiles | Tab::Filtros) {
                        self.move_preset(if mouse.kind == MouseEventKind::ScrollUp {
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
                self.status = format!("Perfil seleccionado: {id}");
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
            Action::Slider(kind) => {
                self.adjust_slider(kind, 0.0);
            }
            Action::Quality(quality) => {
                self.edit_selected(|preset| preset.quality = quality);
                self.status = format!("Calidad DSP configurada a: {}", quality.label());
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
                self.status = "Lista de dispositivos PipeWire actualizada".into();
            }
            Action::SelectDevice(index) => self.select_device(index),
            Action::OpenResetConfirm(target) => {
                self.modal = Some(Modal::ConfirmReset { target });
            }
            Action::OpenCloneFileInput => {
                self.modal = Some(Modal::CloneFileInput {
                    path: String::new(),
                });
            }
            Action::StartCloneRecording => {
                self.send(Request::StartLoopRecord);
                self.clone_session.status = CloneStatus::Validating;
                self.clone_session.status_message =
                    "Grabando muestra de 8-15s en micrófono...".into();
                self.status = "Grabando muestra de voz para inferencia...".into();
            }
            Action::TestCloneVoice => {
                let mut temp = self.clone_session.to_preset();
                temp.id = "temp_clone_test".into();
                temp.name = "Test Clonación IA".into();
                self.send(Request::UpsertPreset {
                    preset: temp,
                    activate: true,
                });
                self.send(Request::StartLoopRecord);
                self.status = "Reproduciendo muestra clonada en Loop Tester".into();
            }
            Action::OpenCloneSaveModal => {
                self.modal = Some(Modal::CloneSaveName {
                    name: self.clone_session.name.clone(),
                });
            }
            Action::DiscardCloneSession => {
                self.clone_session = VoiceCloneSession::default();
                self.status = "Sesión de clonación descartada".into();
            }
            Action::ModalSubmit => self.submit_modal(),
            Action::ModalCancel => self.modal = None,
        }
    }

    fn execute_reset(&mut self, target: ConfigResetTarget) {
        if let Some(client) = self.client.as_mut() {
            let _ = client.reset_config(target);
        }
        self.draft = None;
        self.status = match target {
            ConfigResetTarget::All => "Configuración completa de fábrica restablecida".into(),
            ConfigResetTarget::Audio => "Parámetros de audio y PipeWire restablecidos".into(),
            ConfigResetTarget::Presets => "Presets originales de fábrica restaurados".into(),
            ConfigResetTarget::Dsp => "Efectos y filtros DSP restablecidos".into(),
        };
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
                let value = -12.0 + rel * 24.0;
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
            SliderKind::ClonePitch => {
                self.clone_session.pitch_offset = (-12.0 + rel * 24.0).clamp(-12.0, 12.0);
            }
            SliderKind::CloneFormant => {
                self.clone_session.formant_offset = (-12.0 + rel * 24.0).clamp(-12.0, 12.0);
            }
            SliderKind::CloneBlend => {
                self.clone_session.similarity_blend = rel.clamp(0.0, 1.0);
            }
            SliderKind::CloneDenoise => {
                self.clone_session.denoise_strength = rel.clamp(0.0, 1.0);
            }
        }
    }

    fn adjust_slider(&mut self, kind: SliderKind, delta: f32) {
        match kind {
            SliderKind::Master => self.bump_master(delta),
            SliderKind::PresetGain => self.bump_preset_gain(delta),
            SliderKind::MonitorGain => {
                let cur = self.state.monitor_gain_db;
                let val = (cur + delta).clamp(-24.0, 12.0);
                let enabled = self.state.monitor_enabled;
                self.send(Request::SetMonitor {
                    enabled,
                    gain_db: val,
                });
            }
            SliderKind::Pitch => self.bump_pitch(delta * 0.5),
            SliderKind::Formant => self.bump_formant(delta * 0.5),
            SliderKind::ClonePitch => {
                self.clone_session.pitch_offset =
                    (self.clone_session.pitch_offset + delta * 0.5).clamp(-12.0, 12.0);
            }
            SliderKind::CloneFormant => {
                self.clone_session.formant_offset =
                    (self.clone_session.formant_offset + delta * 0.5).clamp(-12.0, 12.0);
            }
            SliderKind::CloneBlend => {
                self.clone_session.similarity_blend =
                    (self.clone_session.similarity_blend + delta * 0.05).clamp(0.0, 1.0);
            }
            SliderKind::CloneDenoise => {
                self.clone_session.denoise_strength =
                    (self.clone_session.denoise_strength + delta * 0.05).clamp(0.0, 1.0);
            }
            _ => {}
        }
    }

    fn set_master(&mut self, value: f32) {
        self.master_edit = value.clamp(-24.0, 24.0);
        self.master_dirty = true;
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
                    ref mut enabled,
                    ref mut gain_db,
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

    fn bump_formant(&mut self, delta: f32) {
        self.edit_selected(|preset| {
            preset.formant_semitones = (preset.formant_semitones + delta).clamp(-12.0, 12.0);
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

    fn apply_selected(&mut self) {
        let id = self.selected_preset.clone();
        self.send(Request::ApplyPreset { id });
        self.status = "Perfil de voz aplicado al motor PipeWire".into();
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
            self.status = "Instalá primero los modelos neuronales (~30 MB)".into();
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
            self.status = "Solo los perfiles integrados (built-in) se restauran".into();
            return;
        }
        let id = preset.id.clone();
        self.send(Request::ResetPreset { id });
        self.draft = None;
        self.status = "Perfil restaurado a valores de fábrica".into();
    }

    fn delete_selected(&mut self) {
        let Some(preset) = self.selected() else {
            return;
        };
        if preset.builtin {
            self.status = "Un perfil built-in no se puede borrar; usá Restaurar".into();
            return;
        }
        let id = preset.id.clone();
        self.send(Request::DeletePreset { id });
        self.selected_preset = self.state.active_preset.clone();
        self.draft = None;
        self.status = "Perfil personalizado eliminado".into();
    }

    fn toggle_mute(&mut self) {
        let mute = !self.state.muted;
        self.send(Request::SetMute { mute });
        self.status = if mute {
            "MUTE activado (micrófono silenciado)".into()
        } else {
            "MUTE desactivado (micrófono transmitiendo)".into()
        };
    }

    fn toggle_bypass(&mut self) {
        let bypass = !self.state.bypass;
        self.send(Request::SetBypass { bypass });
        self.status = if bypass {
            "BYPASS activado (micrófono limpio directo)".into()
        } else {
            "BYPASS desactivado (procesamiento DSP activo)".into()
        };
    }

    fn toggle_monitor(&mut self) {
        let enabled = !self.state.monitor_enabled;
        self.send(Request::SetMonitor {
            enabled,
            gain_db: self.state.monitor_gain_db,
        });
        self.status = if enabled {
            "🎧 ESCUCHAR MI VOZ: ACTIVADO (Sidetone en auriculares)".into()
        } else {
            "🎧 ESCUCHAR MI VOZ: APAGADO".into()
        };
    }

    fn toggle_loop_test(&mut self) {
        if self.state.loop_test_state == "idle" {
            self.send(Request::StartLoopRecord);
            self.status = "Loop Tester: grabando muestra de 8s...".into();
        } else {
            self.send(Request::StopLoopTest);
            self.status = "Loop Tester: bucle detenido".into();
        }
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
        if !self.state.models_installed {
            self.status = "Modelos neuronales ausentes. Presioná 'i' para instalarlos".into();
            return;
        }
        self.edit_selected(|preset| {
            let was_present = preset.has_filter("speaker_lock");
            if !was_present {
                preset.add_filter("speaker_lock");
            } else {
                preset.filters.iter_mut().for_each(|filter| {
                    if let FilterConfig::SpeakerLock { enabled, .. } = filter {
                        *enabled = !*enabled;
                    }
                });
            }
        });
    }

    fn cycle_profile(&mut self) {
        if self.state.voice_profiles.is_empty() {
            self.status = "No hay perfiles de voz enrolados. Usá Enrolar".into();
            return;
        }
        let current = self
            .selected()
            .and_then(|p| p.speaker_lock())
            .map(|l| l.profile)
            .unwrap_or_default();
        let idx = self
            .state
            .voice_profiles
            .iter()
            .position(|p| p == &current)
            .map(|i| (i + 1) % self.state.voice_profiles.len())
            .unwrap_or(0);
        let next_profile = self.state.voice_profiles[idx].clone();
        self.edit_selected(|preset| {
            if !preset.has_filter("speaker_lock") {
                preset.add_filter("speaker_lock");
            }
            preset.filters.iter_mut().for_each(|f| {
                if let FilterConfig::SpeakerLock {
                    profile, enabled, ..
                } = f
                {
                    *enabled = true;
                    *profile = next_profile.clone();
                }
            });
        });
        self.status = format!("Perfil de voz activo: {next_profile}");
    }

    fn delete_profile(&mut self) {
        let current = self
            .selected()
            .and_then(|p| p.speaker_lock())
            .map(|l| l.profile)
            .unwrap_or_default();
        if current.is_empty() {
            return;
        }
        self.send(Request::DeleteVoice {
            name: current.clone(),
        });
        self.edit_selected(|preset| {
            preset.filters.iter_mut().for_each(|f| {
                if let FilterConfig::SpeakerLock { profile, .. } = f {
                    profile.clear();
                }
            });
        });
        self.status = format!("Perfil de voz eliminado: {current}");
    }

    fn install_models(&mut self) {
        self.send(Request::InstallModels);
        self.status = "Descargando modelos neuronales CAM++ (~30 MB)...".into();
    }

    fn select_device(&mut self, index: usize) {
        if let Some(device) = self.state.devices.get(index) {
            let name = device.name.clone();
            let desc = device.description.clone();
            self.send(Request::SetInput { name });
            self.status = format!("Micrófono seleccionado: {}", desc);
        }
    }

    // ==========================================
    // RENDERIZADO RATATUI UI 3.0
    // ==========================================

    fn draw(&mut self, frame: &mut Frame<'_>) {
        self.hitboxes.clear();
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4), // Header prominente con botón toggle directo de escuchar voz
                Constraint::Length(1), // Barra de pestañas
                Constraint::Min(8),    // Cuerpo del panel según pestaña
                Constraint::Length(2), // Pie con atajos y estado
            ])
            .split(area);

        self.draw_header(frame, chunks[0]);
        self.draw_tabs_bar(frame, chunks[1]);

        match self.tab {
            Tab::Perfiles => self.draw_perfiles(frame, chunks[2]),
            Tab::Filtros => self.draw_filtros(frame, chunks[2]),
            Tab::Dispositivos => self.draw_dispositivos(frame, chunks[2]),
            Tab::Clonacion => self.draw_clonacion(frame, chunks[2]),
            Tab::Ajustes => self.draw_ajustes(frame, chunks[2]),
        }

        self.draw_footer(frame, chunks[3]);

        if self.modal.is_some() {
            self.draw_modal(frame);
        }
    }

    fn draw_header(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .style(Style::default().bg(Color::Rgb(18, 20, 26)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(2)])
            .split(inner);

        // Fila 0: Identidad y Telemetría
        let (running_text, running_color) = if self.state.running {
            ("● PIPEWIRE ACTIVO", Color::Green)
        } else {
            ("○ MOTOR DETENIDO", Color::Red)
        };

        let row0 = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(35),
                Constraint::Percentage(30),
                Constraint::Percentage(35),
            ])
            .split(rows[0]);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " MVX 1.0 ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ModularVoiceX4yi "),
                Span::styled(
                    format!(" {running_text} "),
                    Style::default()
                        .fg(running_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            row0[0],
        );

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("Latencia total: "),
                Span::styled(
                    format!("{:.1} ms", self.state.total_latency_ms),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" (DSP: "),
                Span::styled(
                    format!("{:.1} ms", self.state.dsp_latency_ms),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(")"),
            ]))
            .alignment(Alignment::Center),
            row0[1],
        );

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("Perfil activo: "),
                Span::styled(
                    format!("🎙️ {}", self.state.active_preset_name),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
            ]))
            .alignment(Alignment::Right),
            row0[2],
        );

        // Fila 1: Botón Toggle Escuchar mi voz + Sliders rápidos de audio
        let row1_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30), // Botón Toggle Escuchar mi voz (Sidetone)
                Constraint::Percentage(25), // Volumen Sidetone
                Constraint::Percentage(15), // Botón MUTE
                Constraint::Percentage(15), // Botón BYPASS
                Constraint::Percentage(15), // Volumen Maestro
            ])
            .split(rows[1]);

        // Toggle directo y prominente: Escuchar mi voz
        let (sidetone_label, sidetone_style) = if self.state.monitor_enabled {
            (
                " [ 🎧 ESCUCHAR MI VOZ: ON (S) ] ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (
                " [ 🎧 ESCUCHAR MI VOZ: OFF (S) ] ",
                Style::default()
                    .fg(Color::Gray)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(sidetone_label, sidetone_style)))
                .alignment(Alignment::Center),
            row1_cols[0],
        );
        self.register(row1_cols[0], Action::ToggleMonitor);

        // Slider volumen sidetone
        self.draw_slider(
            frame,
            row1_cols[1],
            "Retorno",
            self.state.monitor_gain_db,
            -24.0,
            12.0,
            "dB",
            SliderKind::MonitorGain,
        );

        // Botón Mute
        let (mute_label, mute_style) = if self.state.muted {
            (
                " [ 🔇 MUTE ACTIVO (M) ] ",
                Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                " [ 🎤 MUTE (M) ] ",
                Style::default().fg(Color::White),
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(mute_label, mute_style)))
                .alignment(Alignment::Center),
            row1_cols[2],
        );
        self.register(row1_cols[2], Action::ToggleMute);

        // Botón Bypass
        let (bypass_label, bypass_style) = if self.state.bypass {
            (
                " [ ⚡ BYPASS ON (B) ] ",
                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                " [ ⚡ BYPASS (B) ] ",
                Style::default().fg(Color::White),
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(bypass_label, bypass_style)))
                .alignment(Alignment::Center),
            row1_cols[3],
        );
        self.register(row1_cols[3], Action::ToggleBypass);

        // Slider Volumen maestro
        self.draw_slider(
            frame,
            row1_cols[4],
            "Master",
            self.master_edit,
            -24.0,
            24.0,
            "dB",
            SliderKind::Master,
        );
    }

    fn draw_tabs_bar(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let tabs = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
            ])
            .split(area);

        self.draw_tab_button(frame, tabs[0], "1 Perfiles", Tab::Perfiles);
        self.draw_tab_button(frame, tabs[1], "2 Filtros / Rack", Tab::Filtros);
        self.draw_tab_button(frame, tabs[2], "3 Dispositivos", Tab::Dispositivos);
        self.draw_tab_button(frame, tabs[3], "4 Clonación IA", Tab::Clonacion);
        self.draw_tab_button(frame, tabs[4], "5 Ajustes & Reset", Tab::Ajustes);
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

    // ==========================================
    // PESTAÑA 1: PERFILES
    // ==========================================

    fn draw_perfiles(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(area);

        // Columna izquierda: Lista de perfiles y botones de gestión
        let block_left = Block::default()
            .borders(Borders::ALL)
            .title(" Biblioteca de Perfiles de Voz ");
        let inner_left = block_left.inner(cols[0]);
        frame.render_widget(block_left, cols[0]);

        if inner_left.height >= 5 {
            let left_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(2), Constraint::Length(1)])
                .split(inner_left);

            let presets = self.state.presets.clone();
            let capacity = left_layout[0].height as usize;
            let selected_index = presets
                .iter()
                .position(|p| p.id == self.selected_preset)
                .unwrap_or(0);
            let start = selected_index.saturating_sub(capacity.saturating_sub(1));

            for (row, preset) in presets.iter().skip(start).take(capacity).enumerate() {
                let y = left_layout[0].y + row as u16;
                let rect = Rect::new(left_layout[0].x, y, left_layout[0].width, 1);
                let selected = preset.id == self.selected_preset;
                let active = preset.id == self.state.active_preset;

                let marker = if active { "●" } else { " " };
                let badge = if preset.builtin {
                    Span::styled(" [BUILT-IN]", Style::default().fg(Color::DarkGray))
                } else {
                    Span::styled(" [CUSTOM]", Style::default().fg(Color::Magenta))
                };

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
                        badge,
                    ])),
                    rect,
                );
                self.register(rect, Action::SelectPreset(preset.id.clone()));
            }

            // Botones: Crear perfil
            self.draw_button(
                frame,
                left_layout[1],
                "＋ Crear nuevo perfil (N)",
                Action::NewPreset,
                true,
            );

            // Botones inferiores: Activar, Restaurar, Borrar
            let action_cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(40),
                    Constraint::Percentage(30),
                    Constraint::Percentage(30),
                ])
                .split(left_layout[2]);

            let is_builtin = self
                .selected()
                .map(|p| p.builtin)
                .unwrap_or(false);

            self.draw_button(
                frame,
                action_cols[0],
                "✓ Activar (Enter)",
                Action::ApplyPreset,
                true,
            );
            self.draw_button(
                frame,
                action_cols[1],
                "↺ Restaurar (R)",
                Action::ResetPreset,
                is_builtin,
            );
            self.draw_button(
                frame,
                action_cols[2],
                "🗑 Borrar (D)",
                Action::DeletePreset,
                !is_builtin,
            );
        }

        // Columna derecha: Inspección y Afinación rápida del perfil seleccionado
        let block_right = Block::default()
            .borders(Borders::ALL)
            .title(" Detalle y Parámetros del Perfil ");
        let inner_right = block_right.inner(cols[1]);
        frame.render_widget(block_right, cols[1]);

        let Some(preset) = self.selected_or_active().cloned() else {
            return;
        };

        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Info banner
                Constraint::Length(1), // Quality selector title
                Constraint::Length(1), // Quality buttons
                Constraint::Length(1), // Sliders title
                Constraint::Length(1), // Pitch
                Constraint::Length(1), // Formant
                Constraint::Length(1), // Preset Gain
                Constraint::Min(4),    // Rack de filtros activos resumen
            ])
            .split(inner_right);

        // Banner informativo
        let status_desc = if preset.id == self.state.active_preset {
            "Perfil actualmente ACTIVO en la salida PipeWire"
        } else {
            "Perfil seleccionado para edición (presioná Enter para activar)"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        preset.name.clone(),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("  (ID: {})", preset.id)),
                ]),
                Line::from(Span::styled(status_desc, Style::default().fg(Color::Gray))),
            ]),
            right_rows[0],
        );

        // Calidad DSP
        frame.render_widget(
            Paragraph::new("Calidad de procesamiento y latencia:").style(Style::default().fg(Color::DarkGray)),
            right_rows[1],
        );

        let q_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(34),
                Constraint::Percentage(33),
            ])
            .split(right_rows[2]);

        for (idx, quality) in [Quality::Ultra, Quality::Balanced, Quality::High]
            .into_iter()
            .enumerate()
        {
            let label = format!("{} (~{:.0}ms)", quality.label(), quality.latency_ms());
            let selected = preset.quality == quality;
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(label, style))).alignment(Alignment::Center),
                q_cols[idx],
            );
            self.register(q_cols[idx], Action::Quality(quality));
        }

        // Sliders de tono y ganancia rápida
        self.draw_slider(
            frame,
            right_rows[4],
            "Tono fundamental (Pitch)",
            preset.pitch_semitones,
            -12.0,
            12.0,
            "st",
            SliderKind::Pitch,
        );
        self.draw_slider(
            frame,
            right_rows[5],
            "Timbre garganta (Formantes)",
            preset.formant_semitones,
            -12.0,
            12.0,
            "st",
            SliderKind::Formant,
        );
        self.draw_slider(
            frame,
            right_rows[6],
            "Ganancia del perfil",
            preset.gain_db(),
            -24.0,
            24.0,
            "dB",
            SliderKind::PresetGain,
        );

        // Resumen de módulos activos en el rack
        let mut active_badges = Vec::new();
        active_badges.push(Span::styled("Filtros en cadena: ", Style::default().fg(Color::DarkGray)));
        for filter in &preset.filters {
            if filter.enabled() {
                active_badges.push(Span::styled(
                    format!(" [✓ {}] ", filter.kind()),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ));
            } else {
                active_badges.push(Span::styled(
                    format!(" [  {}] ", filter.kind()),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(active_badges),
                Line::from(""),
                Line::from("💡 Podés configurar y afinar cada filtro al detalle en la pestaña '2 Filtros'."),
            ])
            .wrap(Wrap { trim: true }),
            right_rows[7],
        );
    }

    // ==========================================
    // PESTAÑA 2: FILTROS (VOICE LAB)
    // ==========================================

    fn draw_filtros(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Rack de Efectos y Filtros: {} ", self.state.active_preset_name));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);

        let Some(preset) = self.selected_or_active().cloned() else {
            return;
        };

        // COLUMNA IZQUIERDA: Rack A - Limpieza, Dinámica y Ecualización
        let left_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // HPF toggle
                Constraint::Length(1), // HPF Cutoff
                Constraint::Length(1), // Gate toggle
                Constraint::Length(1), // Gate threshold
                Constraint::Length(1), // Gate meter status
                Constraint::Length(1), // RNNoise toggle
                Constraint::Length(1), // RNNoise strength
                Constraint::Length(1), // Solo mi voz (CAM++) toggle
                Constraint::Length(1), // Solo mi voz threshold
                Constraint::Length(1), // Solo mi voz profile / enrolar
                Constraint::Length(1), // EQ toggle
                Constraint::Length(1), // EQ Low
                Constraint::Length(1), // EQ Mid
                Constraint::Length(1), // EQ High
                Constraint::Length(1), // Compressor toggle
                Constraint::Length(1), // Comp threshold
                Constraint::Length(1), // Comp ratio
                Constraint::Length(1), // Comp makeup
                Constraint::Min(1),    // Comp reduction meter
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

        // RNNoise
        let nr = preset.noise_reduction();
        self.draw_toggle(
            frame,
            left_rows[5],
            if nr.is_some() {
                "[x] Reducción Neuronal (RNNoise)"
            } else {
                "[ ] Reducción Neuronal (RNNoise)"
            },
            Action::ToggleNoise,
            nr.is_some(),
        );
        self.draw_slider(
            frame,
            left_rows[6],
            "Intensidad NR",
            nr.unwrap_or(0.5),
            0.0,
            1.0,
            "",
            SliderKind::Noise,
        );

        // Solo mi voz (CAM++ Speaker Verification)
        let speaker = preset.speaker_lock();
        self.draw_toggle(
            frame,
            left_rows[7],
            if speaker.is_some() {
                "[x] Solo mi voz (Aislamiento CAM++)"
            } else {
                "[ ] Solo mi voz (Aislamiento CAM++)"
            },
            Action::ToggleSpeaker,
            speaker.is_some(),
        );
        self.draw_slider(
            frame,
            left_rows[8],
            "Umbral CAM++",
            speaker.as_ref().map(|l| l.threshold).unwrap_or(0.72),
            0.50,
            0.99,
            "",
            SliderKind::Threshold,
        );
        let spk_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(left_rows[9]);
        let active_prof = speaker
            .as_ref()
            .map(|l| {
                if l.profile.is_empty() {
                    "(sin perfil)"
                } else {
                    &l.profile
                }
            })
            .unwrap_or("(desactivado)");
        self.draw_button(
            frame,
            spk_cols[0],
            &format!("Perfil: {active_prof}"),
            Action::CycleProfile,
            true,
        );
        self.draw_button(
            frame,
            spk_cols[1],
            "Enrolar",
            Action::EnrollVoice,
            true,
        );
        self.draw_button(
            frame,
            spk_cols[2],
            "Borrar",
            Action::DeleteVoice,
            speaker.as_ref().is_some_and(|l| !l.profile.is_empty()),
        );

        // EQ 3 Bandas
        let eq = preset.equalizer();
        self.draw_toggle(
            frame,
            left_rows[10],
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
            left_rows[11],
            "EQ Graves (100Hz)",
            low,
            -15.0,
            15.0,
            "dB",
            SliderKind::EqLow,
        );
        self.draw_slider(
            frame,
            left_rows[12],
            "EQ Medios (1kHz)",
            mid,
            -15.0,
            15.0,
            "dB",
            SliderKind::EqMid,
        );
        self.draw_slider(
            frame,
            left_rows[13],
            "EQ Agudos (8kHz)",
            high,
            -15.0,
            15.0,
            "dB",
            SliderKind::EqHigh,
        );

        // Compresor
        let comp = preset.compressor();
        self.draw_toggle(
            frame,
            left_rows[14],
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
            left_rows[15],
            "Umbral Compresor",
            comp.as_ref().map(|c| c.threshold_db).unwrap_or(-18.0),
            -40.0,
            0.0,
            "dB",
            SliderKind::CompThreshold,
        );
        self.draw_slider(
            frame,
            left_rows[16],
            "Ratio Compresor",
            comp.as_ref().map(|c| c.ratio).unwrap_or(3.0),
            1.0,
            10.0,
            ":1",
            SliderKind::CompRatio,
        );
        self.draw_slider(
            frame,
            left_rows[17],
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
            left_rows[18],
        );

        // COLUMNA DERECHA: Rack B - Modulación, Carácter y Efectos Creativos
        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Pitch
                Constraint::Length(1), // Formant
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

        // Pitch & Formants
        self.draw_slider(
            frame,
            right_rows[0],
            "Tono (Pitch)",
            preset.pitch_semitones,
            -12.0,
            12.0,
            "st",
            SliderKind::Pitch,
        );
        self.draw_slider(
            frame,
            right_rows[1],
            "Formantes (Timbre)",
            preset.formant_semitones,
            -12.0,
            12.0,
            "st",
            SliderKind::Formant,
        );

        // Robotizer
        let robot = preset.robotizer();
        self.draw_toggle(
            frame,
            right_rows[2],
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
            right_rows[3],
            "Frecuencia Robot",
            rfreq,
            20.0,
            500.0,
            "Hz",
            SliderKind::RobotFreq,
        );
        self.draw_slider(
            frame,
            right_rows[4],
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
            right_rows[5],
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
            right_rows[6],
            "Frecuencia Portadora",
            ring_f,
            50.0,
            2000.0,
            "Hz",
            SliderKind::RingFreq,
        );
        self.draw_slider(
            frame,
            right_rows[7],
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
            right_rows[8],
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
            right_rows[9],
            "Drive Distorsión",
            dist.as_ref().map(|d| d.drive).unwrap_or(6.0),
            0.0,
            24.0,
            "dB",
            SliderKind::DistDrive,
        );
        self.draw_slider(
            frame,
            right_rows[10],
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
            right_rows[11],
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
            right_rows[12],
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
            right_rows[13],
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
            right_rows[14],
            "Tamaño de Sala",
            reverb.as_ref().map(|r| r.room_size).unwrap_or(0.6),
            0.0,
            1.0,
            "",
            SliderKind::ReverbRoom,
        );
        self.draw_slider(
            frame,
            right_rows[15],
            "Mezcla Reverb",
            reverb.as_ref().map(|r| r.mix).unwrap_or(0.3),
            0.0,
            1.0,
            "",
            SliderKind::ReverbMix,
        );
    }

    // ==========================================
    // PESTAÑA 3: DISPOSITIVOS & AUDIO PIPEWIRE
    // ==========================================

    fn draw_dispositivos(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);

        // Columna izquierda: Selección de micrófonos físicos
        let block_left = Block::default()
            .borders(Borders::ALL)
            .title(" Entradas Físicas de Micrófono ");
        let inner_left = block_left.inner(cols[0]);
        frame.render_widget(block_left, cols[0]);

        if inner_left.height >= 4 {
            let left_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(1)])
                .split(inner_left);

            let capacity = left_layout[0].height as usize;
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
                let rect = Rect::new(left_layout[0].x, left_layout[0].y + row as u16, left_layout[0].width, 1);
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

            self.draw_button(
                frame,
                left_layout[1],
                "Actualizar lista de dispositivos PipeWire",
                Action::Rescan,
                true,
            );
        }

        // Columna derecha: Vúmetros, Espectro, Voice Tester Loop y Diagnóstico
        let block_right = Block::default()
            .borders(Borders::ALL)
            .title(" Telemetría y Diagnóstico de PipeWire ");
        let inner_right = block_right.inner(cols[1]);
        frame.render_widget(block_right, cols[1]);

        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // VU Meters In/Out
                Constraint::Length(1), // Espectro 8 bandas
                Constraint::Length(2), // Voice Tester Loop
                Constraint::Length(4), // PipeWire stream details
                Constraint::Min(2),    // Error log
            ])
            .split(inner_right);

        // Vúmetros de Entrada y Salida
        let meter_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(right_rows[0]);

        let in_ratio = db_to_ratio(self.state.input_peak_db);
        let out_ratio = db_to_ratio(self.state.output_peak_db);

        frame.render_widget(
            Gauge::default()
                .gauge_style(Style::default().fg(if self.state.input_peak_db > -3.0 {
                    Color::Red
                } else {
                    Color::Green
                }))
                .ratio(in_ratio)
                .label(format!("In Peak: {:.1} dB", self.state.input_peak_db)),
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
                .label(format!("Out Peak: {:.1} dB", self.state.output_peak_db)),
            meter_cols[1],
        );

        // Espectro de 8 bandas
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
        frame.render_widget(Paragraph::new(Line::from(spectrum_spans)), right_rows[1]);

        // Voice Tester Loop
        let loop_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(right_rows[2]);

        let (loop_text, loop_color) = match self.state.loop_test_state.as_str() {
            "recording" => ("● Grabando 8s de voz...", Color::Red),
            "looping" => ("▶ Reproduciendo bucle continuo", Color::Green),
            _ => ("Loop Tester: inactivo", Color::Gray),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                loop_text,
                Style::default().fg(loop_color).add_modifier(Modifier::BOLD),
            ))),
            loop_cols[0],
        );

        let btn_label = if self.state.loop_test_state == "idle" {
            "● Grabar bucle 8s (L)"
        } else {
            "■ Parar bucle (L)"
        };
        self.draw_button(
            frame,
            loop_cols[1],
            btn_label,
            Action::ToggleLoopRecord,
            true,
        );

        // Detalles de Stream PipeWire
        let input_desc = self
            .state
            .selected_input_description
            .clone()
            .or_else(|| self.state.selected_input.clone())
            .unwrap_or_else(|| "(ninguna)".into());

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Dispositivo: {input_desc}")),
                Line::from(format!(
                    "Fuente virtual: {} · {} Hz · {} ch",
                    self.state.virtual_source, self.state.sample_rate, self.state.channels
                )),
                Line::from(format!(
                    "Buffer Quantum: {} frames · Xruns: {} · Overflows: {}",
                    self.state.quantum, self.state.xruns, self.state.overflows
                )),
            ])
            .style(Style::default().fg(Color::DarkGray)),
            right_rows[3],
        );

        let error_msg = self
            .state
            .error
            .clone()
            .map(|e| format!("AVISO: {e}"))
            .unwrap_or_else(|| "Estado de audio PipeWire: normal, sin fallos".into());
        frame.render_widget(
            Paragraph::new(error_msg).style(Style::default().fg(Color::Gray)),
            right_rows[4],
        );
    }

    // ==========================================
    // PESTAÑA 4: CLONACIÓN DE VOCES (IA / NEURONAL)
    // ==========================================

    fn draw_clonacion(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Laboratorio de Clonación de Voces (Redes Neuronales / IA) ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Paso 1: Selección de muestra
                Constraint::Length(3), // Paso 2: Inferencia & Embeddings
                Constraint::Length(6), // Paso 3: Calibración y pulido
                Constraint::Length(3), // Paso 4: Acciones (Preescuchar / Guardar)
                Constraint::Min(2),    // Información y notas
            ])
            .split(inner);

        // Paso 1: Muestra de Audio
        let p1_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(rows[0]);

        let path_text = if self.clone_session.source_path.as_os_str().is_empty() {
            "(ninguna muestra cargada; usá los botones a la derecha)".to_string()
        } else {
            self.clone_session
                .source_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        };

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Paso 1: Muestra de la voz a clonar",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("Audio: {path_text}")),
            ]),
            p1_cols[0],
        );

        self.draw_button(
            frame,
            p1_cols[1],
            "📁 Cargar archivo WAV/MP3",
            Action::OpenCloneFileInput,
            true,
        );

        self.draw_button(
            frame,
            p1_cols[2],
            "🎙 Grabar muestra (15s)",
            Action::StartCloneRecording,
            true,
        );

        // Paso 2: Estado de la Inferencia Neuronal
        let (status_badge, status_color) = match self.clone_session.status {
            CloneStatus::Idle => ("EN ESPERA", Color::DarkGray),
            CloneStatus::Validating => ("VALIDANDO AUDIO...", Color::Yellow),
            CloneStatus::Extracting => ("EXTRAYENDO EMBEDDINGS NEURONALES...", Color::Yellow),
            CloneStatus::Ready => ("MUESTRA PROCESADA Y LISTA", Color::Green),
            CloneStatus::Synthesizing => ("SINTETIZANDO TIMBRE VOCAL...", Color::Cyan),
            CloneStatus::Error => ("ERROR DE EXTRACCIÓN", Color::Red),
        };

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        "Paso 2: Inferencia Neuronal ",
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" [{status_badge}] "),
                        Style::default().fg(status_color).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(format!(
                    "Motor: {} · {}",
                    self.clone_session.model_target, self.clone_session.status_message
                )),
            ]),
            rows[1],
        );

        // Paso 3: Calibración y Afinación
        let p3_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(rows[2]);

        frame.render_widget(
            Paragraph::new(Span::styled(
                "Paso 3: Calibración de Timbre y Afinación Fina",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            p3_rows[0],
        );

        self.draw_slider(
            frame,
            p3_rows[1],
            "Afinación Tono (Pitch Target)",
            self.clone_session.pitch_offset,
            -12.0,
            12.0,
            "st",
            SliderKind::ClonePitch,
        );
        self.draw_slider(
            frame,
            p3_rows[2],
            "Escala de Formantes (Tracto Vocal)",
            self.clone_session.formant_offset,
            -12.0,
            12.0,
            "st",
            SliderKind::CloneFormant,
        );
        self.draw_slider(
            frame,
            p3_rows[3],
            "Mezcla de Similitud (Blend IA)",
            self.clone_session.similarity_blend,
            0.0,
            1.0,
            "",
            SliderKind::CloneBlend,
        );
        self.draw_slider(
            frame,
            p3_rows[4],
            "Limpieza de Ruido de Fondo",
            self.clone_session.denoise_strength,
            0.0,
            1.0,
            "",
            SliderKind::CloneDenoise,
        );

        // Paso 4: Botones de Acción
        let p4_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(34),
                Constraint::Percentage(33),
            ])
            .split(rows[3]);

        self.draw_button(
            frame,
            p4_cols[0],
            "▶ Preescuchar en bucle",
            Action::TestCloneVoice,
            true,
        );
        self.draw_button(
            frame,
            p4_cols[1],
            "💾 Guardar como perfil de voz",
            Action::OpenCloneSaveModal,
            true,
        );
        self.draw_button(
            frame,
            p4_cols[2],
            "✖ Descartar sesión",
            Action::DiscardCloneSession,
            true,
        );

        // Información adicional
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("💡 Podés cargar cualquier archivo de audio limpio o grabar tu voz para clonar."),
                Line::from("   El extractor analiza los coeficientes acústicos y genera un perfil listo para usar en Discord / OBS."),
            ])
            .style(Style::default().fg(Color::DarkGray)),
            rows[4],
        );
    }

    // ==========================================
    // PESTAÑA 5: AJUSTES & RESET DE CONFIGURACIÓN
    // ==========================================

    fn draw_ajustes(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Ajustes Globales y Manejo de Configuraciones ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5), // Rutas del sistema
                Constraint::Length(1), // Título resets
                Constraint::Length(2), // Botón Reset Fábrica Completo
                Constraint::Length(2), // Botones resets específicos
                Constraint::Length(1), // Título modelos
                Constraint::Length(2), // Botón modelos
                Constraint::Min(2),    // Control del daemon
            ])
            .split(inner);

        // Rutas del sistema
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Configuración: ", Style::default().fg(Color::Gray)),
                    Span::raw(mvx_core::config_path().display().to_string()),
                ]),
                Line::from(vec![
                    Span::styled("Presets:       ", Style::default().fg(Color::Gray)),
                    Span::raw(mvx_core::presets_path().display().to_string()),
                ]),
                Line::from(vec![
                    Span::styled("Modelos ONNX:  ", Style::default().fg(Color::Gray)),
                    Span::raw(mvx_core::models_dir().display().to_string()),
                ]),
                Line::from(vec![
                    Span::styled("IPC Socket:    ", Style::default().fg(Color::Gray)),
                    Span::raw(socket_path().display().to_string()),
                ]),
            ]),
            rows[0],
        );

        // Resets
        frame.render_widget(
            Paragraph::new("Restablecimiento de Configuraciones:")
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            rows[1],
        );

        self.draw_button(
            frame,
            rows[2],
            "⚠ Restablecer Configuración Completa de Fábrica (Reset Global)",
            Action::OpenResetConfirm(ConfigResetTarget::All),
            true,
        );

        let reset_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(34),
                Constraint::Percentage(33),
            ])
            .split(rows[3]);

        self.draw_button(
            frame,
            reset_cols[0],
            "↺ Reset Solo Audio / PipeWire",
            Action::OpenResetConfirm(ConfigResetTarget::Audio),
            true,
        );

        self.draw_button(
            frame,
            reset_cols[1],
            "↺ Reset Solo Presets / Perfiles",
            Action::OpenResetConfirm(ConfigResetTarget::Presets),
            true,
        );

        self.draw_button(
            frame,
            reset_cols[2],
            "↺ Reset Filtros DSP",
            Action::OpenResetConfirm(ConfigResetTarget::Dsp),
            true,
        );

        // Modelos
        frame.render_widget(
            Paragraph::new("Modelos Neuronales de Voz:")
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            rows[4],
        );

        let models_label = if self.state.models_installed {
            "✓ Modelos Neuronales Instalados (CAM++ & Silero VAD)"
        } else {
            "⬇ Descargar e Instalar Modelos Neuronales (~30 MB)"
        };
        self.draw_button(frame, rows[5], models_label, Action::InstallModels, true);

        // Control del daemon
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from("💡 Para apagar el servicio por completo podés presionar 'q' o ejecutar `mvx shutdown`."),
            ])
            .style(Style::default().fg(Color::DarkGray)),
            rows[6],
        );
    }

    // ==========================================
    // FOOTER & MODALES
    // ==========================================

    fn draw_footer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let help = Line::from(vec![
            Span::styled(" q/Esc ", key_style()),
            Span::raw("Salir "),
            Span::styled(" 1-5/Tab ", key_style()),
            Span::raw("Pestañas "),
            Span::styled(" s ", key_style()),
            Span::raw("Escuchar voz "),
            Span::styled(" m ", key_style()),
            Span::raw("Mute "),
            Span::styled(" b ", key_style()),
            Span::raw("Bypass "),
            Span::styled(" l ", key_style()),
            Span::raw("Loop test "),
            Span::styled(" n ", key_style()),
            Span::raw("Nuevo perfil "),
            Span::styled(" +/- ", key_style()),
            Span::raw("Volumen"),
        ]);
        let status = Line::from(vec![
            Span::styled(" Estado: ", Style::default().fg(Color::Gray)),
            Span::raw(self.status.clone()),
        ]);
        frame.render_widget(Paragraph::new(vec![help, status]), area);
    }

    fn draw_modal(&mut self, frame: &mut Frame<'_>) {
        let Some(modal) = self.modal.clone() else {
            return;
        };
        let area = frame.area();
        let width = area.width.saturating_sub(10).clamp(36, 70);
        let height = 9;
        let rect = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, rect);

        let border_color = match modal {
            Modal::ConfirmReset { .. } => Color::Red,
            _ => Color::Cyan,
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", modal.title()))
            .style(Style::default().bg(Color::Black).fg(border_color));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let lines = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(modal.hint())
                .style(Style::default().fg(Color::White))
                .wrap(Wrap { trim: true }),
            lines[0],
        );

        if !matches!(modal, Modal::ConfirmReset { .. }) {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan)),
                    Span::raw(modal.value().to_string()),
                    Span::styled("▏", Style::default().fg(Color::Cyan)),
                ]))
                .style(Style::default().fg(Color::White)),
                lines[2],
            );
        }

        let buttons = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(lines[3]);

        self.draw_button(frame, buttons[0], "Cancelar (Esc)", Action::ModalCancel, true);
        self.draw_button(frame, buttons[1], "Confirmar (Enter)", Action::ModalSubmit, true);
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
        let formatted_label = match unit {
            "dB" | "st" => format!("{label}: {value:+.1} {unit}"),
            "Hz" | "ms" => format!("{label}: {value:.0} {unit}"),
            ":1" => format!("{label}: {value:.1}:1"),
            "" => format!("{label}: {value:.2}"),
            _ => format!("{label}: {value:.2} {unit}"),
        };
        frame.render_widget(
            Gauge::default()
                .ratio(ratio)
                .label(formatted_label)
                .gauge_style(
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
