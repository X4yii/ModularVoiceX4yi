use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use mvx_core::{socket_path, Snapshot};
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
    /// Ask the daemon to exit.
    Shutdown,
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
    let me = std::env::current_exe().ok();
    let candidate = me.as_ref().and_then(|p| p.parent()).map(|p| p.join("mvxd"));
    let mut command = if let Some(path) = candidate.filter(|p| p.exists()) {
        Command::new(path)
    } else {
        Command::new("mvxd")
    };
    let log_path = mvx_core::runtime_dir().join("mvxd.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
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
    command
        .spawn()
        .context("no se pudo iniciar mvxd; instalalo en PATH o iniciá el servicio systemd")?;
    Ok(())
}

fn run_tui() -> Result<()> {
    let mut app = App::new();
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

struct App {
    client: Option<Client>,
    state: Snapshot,
    selected: usize,
    status: String,
    last_poll: Instant,
    last_start: Instant,
    quit: bool,
    gain_edit: f32,
}

impl App {
    fn new() -> Self {
        Self {
            client: None,
            state: Snapshot::initial(),
            selected: 0,
            status: "conectando…".into(),
            last_poll: Instant::now() - Duration::from_secs(10),
            last_start: Instant::now() - Duration::from_secs(10),
            quit: false,
            gain_edit: 0.0,
        }
    }

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        self.ensure_client();
        while !self.quit {
            if self.last_poll.elapsed() > Duration::from_millis(120) {
                self.poll();
                self.last_poll = Instant::now();
            }
            terminal.draw(|frame| self.draw(frame))?;

            if event::poll(Duration::from_millis(30))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.handle_key(key.code);
                    }
                }
            }
        }
        Ok(())
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
                    self.selected = state
                        .devices
                        .iter()
                        .position(|d| Some(d.name.as_str()) == state.selected_input.as_deref())
                        .unwrap_or(0);
                    self.gain_edit = state.gain_db;
                    self.state = state;
                }
                self.status = "conectado a mvxd".into();
                self.client = Some(client);
            }
            Err(err) => {
                self.status = format!("sin daemon: {err}");
            }
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
                if self.selected >= self.state.devices.len() {
                    self.selected = self.state.devices.len().saturating_sub(1);
                }
                self.gain_edit = self.state.gain_db;
            }
            Err(err) => {
                self.client = None;
                self.status = format!("daemon desconectado: {err}");
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.state.devices.is_empty() {
                    self.selected = (self.selected + 1).min(self.state.devices.len() - 1);
                }
            }
            KeyCode::Enter => {
                if let Some((name, description)) = self
                    .state
                    .devices
                    .get(self.selected)
                    .map(|d| (d.name.clone(), d.description.clone()))
                {
                    self.send(Request::SetInput { name });
                    self.status = format!("entrada: {description}");
                }
            }
            KeyCode::Char('m') => {
                let mute = !self.state.muted;
                self.send(Request::SetMute { mute });
                self.status = if mute {
                    "mute ON".into()
                } else {
                    "mute OFF".into()
                };
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.gain_edit = (self.gain_edit + 1.0).clamp(-60.0, 24.0);
                self.send(Request::SetGain {
                    gain_db: self.gain_edit,
                });
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                self.gain_edit = (self.gain_edit - 1.0).clamp(-60.0, 24.0);
                self.send(Request::SetGain {
                    gain_db: self.gain_edit,
                });
            }
            KeyCode::Char('r') => {
                self.send(Request::GetState);
                self.status = "estado actualizado".into();
            }
            _ => {}
        }
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

    fn draw(&self, frame: &mut Frame<'_>) {
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
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(outer[1]);
        self.draw_devices(frame, body[0]);
        self.draw_status(frame, body[1]);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" q/Esc ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw("salir  "),
                Span::styled(" j/k ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw("elegir  "),
                Span::styled(" Enter ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw("aplicar  "),
                Span::styled(" m ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw("mute  "),
                Span::styled(" +/- ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw("gain"),
            ])),
            outer[2],
        );
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
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
                "   daemon v{}   protocolo {}",
                self.state.daemon_version, self.state.protocol_version
            )),
        ]);
        frame.render_widget(
            Paragraph::new(title).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn draw_devices(&self, frame: &mut Frame<'_>, area: Rect) {
        let items: Vec<ListItem> = self
            .state
            .devices
            .iter()
            .map(|device| {
                let selected = self
                    .state
                    .selected_input
                    .as_deref()
                    .map(|s| s == device.name)
                    .unwrap_or(false);
                let prefix = if selected { "● " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(
                        device.description.clone(),
                        if selected {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::raw(format!("  [{}]", device.name)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Entrada física "),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut state = ratatui::widgets::ListState::default();
        if !self.state.devices.is_empty() {
            state.select(Some(self.selected.min(self.state.devices.len() - 1)));
        }
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),
                Constraint::Length(4),
                Constraint::Length(4),
                Constraint::Min(3),
            ])
            .split(area);

        let input = self
            .state
            .selected_input_description
            .clone()
            .or_else(|| self.state.selected_input.clone())
            .unwrap_or_else(|| "(sin entrada)".into());
        let lines = vec![
            Line::from(vec![
                Span::styled("Entrada:  ", Style::default().fg(Color::Gray)),
                Span::raw(input),
            ]),
            Line::from(vec![
                Span::styled("Salida:   ", Style::default().fg(Color::Gray)),
                Span::raw(format!("{} (virtual)", self.state.virtual_source)),
            ]),
            Line::from(vec![
                Span::styled("Formato:  ", Style::default().fg(Color::Gray)),
                Span::raw(format!(
                    "{} Hz · {} ch",
                    self.state.sample_rate, self.state.channels
                )),
            ]),
            Line::from(vec![
                Span::styled("Quantum:  ", Style::default().fg(Color::Gray)),
                Span::raw(format!(
                    "{} frames · latencia est. {:.2} ms",
                    self.state.quantum, self.state.latency_ms
                )),
            ]),
            Line::from(vec![
                Span::styled("Gain:     ", Style::default().fg(Color::Gray)),
                Span::raw(format!(
                    "{:+.1} dB{}",
                    self.state.gain_db,
                    if self.state.muted { " · MUTE" } else { "" }
                )),
            ]),
            Line::from(vec![
                Span::styled("Xruns:    ", Style::default().fg(Color::Gray)),
                Span::raw(format!(
                    "{} · underruns {} · overflows {}",
                    self.state.xruns, self.state.underruns, self.state.overflows
                )),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(" Estado "))
                .wrap(Wrap { trim: true }),
            chunks[0],
        );

        let in_ratio = db_to_ratio(self.state.input_peak_db);
        let out_ratio = db_to_ratio(self.state.output_peak_db);
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
            chunks[1],
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
            chunks[2],
        );
        let error = self
            .state
            .error
            .clone()
            .map(|e| format!("ERROR: {e}"))
            .unwrap_or_else(|| "Sin errores".into());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(self.status.clone()),
                Line::from(error),
                Line::from(""),
                Line::from("M0: PipeWire nativo · micrófono virtual · passthrough con gain/mute"),
            ])
            .block(Block::default().borders(Borders::ALL).title(" Mensajes ")),
            chunks[3],
        );
    }
}

fn db_to_ratio(db: f32) -> f64 {
    (((db + 60.0) / 60.0).clamp(0.0, 1.0)) as f64
}

fn doctor() -> Result<()> {
    let path = socket_path();
    println!("MVX doctor");
    println!("  socket: {}", path.display());
    println!("  config: {}", mvx_core::config_path().display());
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
                "  [ok] quantum: {} ({:.2} ms est.)",
                state.quantum, state.latency_ms
            );
            println!(
                "  [{}] xruns: {}",
                if state.xruns == 0 { "ok" } else { "!!" },
                state.xruns
            );
            println!(
                "  [{}] underruns: {}",
                if state.underruns == 0 { "ok" } else { "!!" },
                state.underruns
            );
            println!(
                "  [{}] overflows: {}",
                if state.overflows == 0 { "ok" } else { "!!" },
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
