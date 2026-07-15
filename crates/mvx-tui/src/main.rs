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
    /// Print one state snapshot.
    Status,
    /// List available input devices.
    Devices,
    /// Diagnose the daemon and audio path.
    Doctor,
    /// Manage speaker-verification models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Ask the daemon to exit.
    Shutdown,
}

#[derive(Debug, Subcommand)]
enum ModelsAction {
    /// Download the CAM++ speaker model and Silero VAD model.
    Install,
    /// Show whether the models are installed.
    Status,
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command.unwrap_or(CliCommand::Tui) {
        CliCommand::Tui => run_tui(),
        CliCommand::Status => {
            let mut client = connect_or_start()?;
            let state = client.state()?;
            println!("{}", serde_json::to_string_pretty(&state)?);
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

    start_daemon()?;
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

/// Start a helper in its own session so closing the terminal does not kill it.
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
    // Always re-assert both mouse tracking and SGR reporting. Some terminals
    // drop one of these modes after a focus change, resize or a child process
    // has changed terminal modes.
    execute!(io::stdout(), EnableMouseCapture, EnableFocusChange)
}

fn disable_mouse_capture() {
    let _ = execute!(io::stdout(), DisableFocusChange, DisableMouseCapture);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    General,
    Devices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SliderKind {
    Master,
    PresetGain,
    Noise,
    Threshold,
    Hold,
    Lookahead,
    Pitch,
    Formant,
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
    Slider(SliderKind),
    Quality(Quality),
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
            tab: Tab::General,
            selected_device: 0,
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
                        // Do not force a preset write on every drag event: the
                        // normal throttle coalesces them and the final value is
                        // flushed on mouse-up.
                        self.flush_edits(false);
                    }
                    // A terminal may reset mouse reporting when it regains
                    // focus or changes size. Re-assert the modes instead of
                    // waiting for the user to restart the TUI.
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
                    Tab::General => Tab::Devices,
                    Tab::Devices => Tab::General,
                };
            }
            KeyCode::Char('g') => self.tab = Tab::General,
            KeyCode::Char('d') => self.tab = Tab::Devices,
            KeyCode::Up | KeyCode::Char('k') if self.tab == Tab::General => {
                self.move_preset(-1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.tab == Tab::General => {
                self.move_preset(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_device = self.selected_device.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.state.devices.is_empty() {
                    self.selected_device =
                        (self.selected_device + 1).min(self.state.devices.len() - 1);
                }
            }
            KeyCode::Enter if self.tab == Tab::General => self.apply_selected(),
            KeyCode::Enter => self.select_device(self.selected_device),
            KeyCode::Char('n') => self.open_new_preset(),
            KeyCode::Char('r') => self.reset_selected(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('s') => self.toggle_speaker(),
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
                    } else if self.tab == Tab::General {
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
            Action::Slider(kind) => {
                self.adjust_slider(kind, 0.0);
            }
            Action::Quality(quality) => {
                self.edit_selected(|preset| preset.quality = quality);
                self.status = format!("calidad DSP: {}", quality.label());
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
        }
    }

    fn adjust_slider(&mut self, kind: SliderKind, direction: f32) {
        match kind {
            SliderKind::Master => self.bump_master(direction),
            SliderKind::PresetGain => self.bump_preset_gain(direction),
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
            Tab::General => self.draw_general(frame, outer[1]),
            Tab::Devices => self.draw_devices(frame, outer[1]),
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
            .constraints([Constraint::Min(30), Constraint::Length(26)])
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
        let title = Line::from(vec![
            Span::styled(
                " MVX ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ModularVoiceX4yi  "),
            Span::styled(
                running,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "   {:.1} ms   preset: {}",
                self.state.total_latency_ms, self.state.active_preset_name
            )),
        ]);
        frame.render_widget(Paragraph::new(title), chunks[0]);

        let tabs = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);
        self.draw_tab_button(frame, tabs[0], "General", Tab::General);
        self.draw_tab_button(frame, tabs[1], "Dispositivos", Tab::Devices);
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

    fn draw_general(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(27),
                Constraint::Percentage(40),
                Constraint::Percentage(33),
            ])
            .split(area);
        self.draw_presets(frame, chunks[0]);
        self.draw_filters(frame, chunks[1]);
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

    fn draw_filters(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Filtros y audio ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 8 {
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
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

        self.draw_slider(
            frame,
            rows[0],
            "Volumen maestro",
            self.master_edit,
            -24.0,
            24.0,
            "dB",
            SliderKind::Master,
        );
        let preset_gain = preset.gain_db();
        self.draw_slider(
            frame,
            rows[1],
            "Ganancia del preset",
            preset_gain,
            -24.0,
            24.0,
            "dB",
            SliderKind::PresetGain,
        );
        let mute_label = if self.state.muted {
            "[x] Silenciar"
        } else {
            "[ ] Silenciar"
        };
        self.draw_toggle(
            frame,
            rows[2],
            mute_label,
            Action::ToggleMute,
            self.state.muted,
        );

        let nr = preset.noise_reduction();
        let nr_label = if nr.is_some() {
            "[x] Reducción de ruido (RNNoise)"
        } else {
            "[ ] Reducción de ruido (RNNoise)"
        };
        self.draw_toggle(frame, rows[3], nr_label, Action::ToggleNoise, nr.is_some());
        self.draw_slider(
            frame,
            rows[4],
            "Intensidad NR",
            nr.unwrap_or(0.0),
            0.0,
            1.0,
            "",
            SliderKind::Noise,
        );

        let speaker = preset.speaker_lock();
        let speaker_label = if speaker.is_some() {
            "[x] Solo mi voz"
        } else {
            "[ ] Solo mi voz"
        };
        self.draw_toggle(
            frame,
            rows[5],
            speaker_label,
            Action::ToggleSpeaker,
            speaker.is_some(),
        );
        let status = if let Some(lock) = &speaker {
            format!(
                "Perfil: {}\n{} · score {:.2} · VAD {} · objetivo {}",
                if lock.profile.is_empty() {
                    "(ninguno)"
                } else {
                    &lock.profile
                },
                self.state.speaker_status,
                self.state.speaker_score,
                if self.state.speaker_vad { "sí" } else { "no" },
                if self.state.speaker_target {
                    "abierto"
                } else {
                    "cerrado"
                },
            )
        } else {
            self.state.speaker_status.clone()
        };
        frame.render_widget(
            Paragraph::new(status)
                .style(Style::default().fg(Color::Gray))
                .wrap(Wrap { trim: true }),
            rows[6],
        );
        let profile_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(rows[7]);
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
            "Enrolar voz",
            Action::EnrollVoice,
            true,
        );

        self.draw_slider(
            frame,
            rows[8],
            "Umbral de coincidencia",
            speaker.as_ref().map(|lock| lock.threshold).unwrap_or(0.72),
            0.50,
            0.99,
            "",
            SliderKind::Threshold,
        );
        self.draw_slider(
            frame,
            rows[9],
            "Retención de voz",
            speaker
                .as_ref()
                .map(|lock| lock.hold_ms as f32)
                .unwrap_or(300.0),
            50.0,
            1500.0,
            "ms",
            SliderKind::Hold,
        );
        self.draw_slider(
            frame,
            rows[10],
            "Lookahead",
            speaker
                .as_ref()
                .map(|lock| lock.lookahead_ms as f32)
                .unwrap_or(100.0),
            0.0,
            300.0,
            "ms",
            SliderKind::Lookahead,
        );
        if !self.state.models_installed {
            self.draw_button(
                frame,
                rows[11],
                "Instalar modelos de voz (~30 MB)",
                Action::InstallModels,
                true,
            );
        } else {
            let has_profile = speaker
                .as_ref()
                .is_some_and(|lock| !lock.profile.trim().is_empty());
            self.draw_button(
                frame,
                rows[11],
                "Borrar perfil de voz",
                Action::DeleteVoice,
                has_profile,
            );
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "Latencia: base {:.1} + DSP {:.1} = {:.1} ms",
                    self.state.latency_ms, self.state.dsp_latency_ms, self.state.total_latency_ms
                )),
                Line::from(format!(
                    "Quantum {} · {} Hz · xruns {}",
                    self.state.quantum, self.state.sample_rate, self.state.xruns
                )),
            ])
            .style(Style::default().fg(Color::DarkGray)),
            rows[12],
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
                Line::from("El pitch cambia la altura de la voz."),
                Line::from("Los formantes cambian el tamaño aparente"),
                Line::from("de la garganta sin mover el pitch."),
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
            Span::raw("salir  "),
            Span::styled(" Tab ", key_style()),
            Span::raw("sección  "),
            Span::styled(
                if self.mouse_active {
                    " ratón "
                } else {
                    " teclado "
                },
                key_style(),
            ),
            Span::raw(if self.mouse_active {
                "clic/rueda  "
            } else {
                "ratón no disponible  "
            }),
            Span::styled(" n ", key_style()),
            Span::raw("nuevo  "),
            Span::styled(" e ", key_style()),
            Span::raw("enrolar  "),
            Span::styled(" s ", key_style()),
            Span::raw("solo mi voz  "),
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
            "ms" => format!("{label}: {value:.0} {unit}"),
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

    /// A toggle is always clickable; `active` only changes its appearance.
    /// Previously the checkbox state was passed as `enabled`, which made the
    /// control impossible to turn on with the mouse when it was off.
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
            println!("podés seguir el estado con `mxv status` o en la TUI.");
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
                    "faltan; ejecutá `mxv models install`"
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

#[allow(dead_code)]
fn _is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.metadata()
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

#[allow(dead_code)]
fn _io_context<T>(result: io::Result<T>) -> Result<T> {
    result.context("operación de terminal falló")
}
