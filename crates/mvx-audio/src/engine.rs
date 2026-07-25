use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as AnyhowContext, Result};
use pipewire as pw;
use pipewire::proxy::ProxyT;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::audio::AudioInfoRaw;
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::Pod;
use rtrb::{Consumer, Producer, RingBuffer};
use tracing::{debug, warn};

use mvx_core::{Config, DeviceInfo, Preset, PresetStore, SharedState};

use crate::dsp::{DspChain, DspParams};
use crate::speaker::SpeakerWorker;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const RING_CAPACITY: usize = 16_384;
const TICK: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub enum AudioCommand {
    SetInput(String),
    SetGain(f32),
    SetMute(bool),
    ApplyPreset { id: String },
    UpsertPreset { preset: Preset, activate: bool },
    DeletePreset { id: String },
    ResetPreset { id: String },
    EnrollVoice { name: String, seconds: u32 },
    DeleteVoice { name: String },
    ReloadAudio,
    Rescan,
    Tick,
    Shutdown,
}

pub type CommandSender = pw::channel::Sender<AudioCommand>;

pub struct AudioEngine {
    inner: Rc<RefCell<Engine>>,
    mainloop: pw::main_loop::MainLoopRc,
    command_rx: Option<pw::channel::Receiver<AudioCommand>>,
    sender: CommandSender,
    tick_stop: Arc<AtomicBool>,
}

impl AudioEngine {
    pub fn new(shared: Arc<SharedState>, config: Config) -> Result<Self> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)
            .context("no se pudo crear el MainLoop de PipeWire")?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .context("no se pudo crear el contexto de PipeWire")?;
        let core = context
            .connect_rc(None)
            .context("no se pudo conectar al core de PipeWire")?;
        let registry = core
            .get_registry_rc()
            .context("no se pudo obtener el registry de PipeWire")?;

        let (sender, command_rx) = pw::channel::channel();

        let engine = Engine::new(mainloop.clone(), core, registry, shared, config)?;

        Ok(Self {
            inner: Rc::new(RefCell::new(engine)),
            mainloop,
            command_rx: Some(command_rx),
            sender,
            tick_stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn sender(&self) -> CommandSender {
        self.sender.clone()
    }

    pub fn run(mut self) -> Result<()> {
        let rx = self
            .command_rx
            .take()
            .ok_or_else(|| anyhow!("engine ya iniciado"))?;

        let inner = self.inner.clone();
        let _attached = rx.attach(self.mainloop.loop_(), move |cmd| {
            if let Ok(mut engine) = inner.try_borrow_mut() {
                engine.handle_command(cmd);
            }
        });

        let tick_stop = self.tick_stop.clone();
        let tick_sender = self.sender.clone();
        let ticker = std::thread::Builder::new()
            .name("mvx-tick".into())
            .spawn(move || {
                while !tick_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(TICK);
                    if tick_sender.send(AudioCommand::Tick).is_err() {
                        break;
                    }
                }
            })
            .context("no se pudo crear el hilo de tick")?;

        self.mainloop.run();
        self.tick_stop.store(true, Ordering::Relaxed);
        let _ = ticker.join();
        if let Ok(engine) = self.inner.try_borrow() {
            engine.shared.set_running(false);
        }
        Ok(())
    }
}

struct Engine {
    // Drop order matters: stop audio objects before the core/loop.
    capture: Option<Capture>,
    playback: Option<Playback>,
    links: Vec<pw::link::Link>,
    linked_ports: HashSet<(u32, u32)>,
    _virtual_listener: Option<pw::proxy::ProxyListener>,
    _virtual_node: Option<pw::node::Node>,
    virtual_node_id: Option<u32>,
    _registry_listener: pw::registry::Listener,
    _registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    mainloop: pw::main_loop::MainLoopRc,

    shared: Arc<SharedState>,
    config: Config,
    nodes: Rc<RefCell<HashMap<u32, NodeMeta>>>,
    ports: Rc<RefCell<HashMap<u32, PortMeta>>>,
    dirty: Rc<Cell<bool>>,
    selected_input: Option<String>,
    presets: PresetStore,
    active_preset: Preset,
    master_gain_db: f32,
    gain: Arc<AtomicU32>,
    mute: Arc<AtomicBool>,
    pitch_control: Arc<AtomicU32>,
    formant_control: Arc<AtomicU32>,
    noise_strength: Arc<AtomicU32>,
    noise_enabled: Arc<AtomicBool>,
    speaker_lookahead_ms: Arc<AtomicU32>,
    bypass: Arc<AtomicBool>,
    speaker: Option<SpeakerWorker>,
    last_retry: Instant,
    link_warned: bool,
    presets_dirty: bool,
    last_presets_save: Instant,
}

#[derive(Clone, Debug)]
struct NodeMeta {
    id: u32,
    name: String,
    description: String,
    media_class: String,
}

#[derive(Clone, Debug)]
struct PortMeta {
    id: u32,
    node_id: u32,
    name: String,
    direction: String,
    channel: Option<String>,
}

struct Capture {
    _listener: pw::stream::StreamListener<CaptureData>,
    _stream: pw::stream::StreamRc,
}

struct Playback {
    _listener: pw::stream::StreamListener<PlaybackData>,
    _stream: pw::stream::StreamRc,
    node_id: u32,
}

struct CaptureData {
    producer: Producer<f32>,
    channels: usize,
    dsp: DspChain,
    mono: Vec<f32>,
    processed: Vec<f32>,
    speaker: Option<Producer<f32>>,
    shared: Arc<SharedState>,
}

struct PlaybackData {
    consumer: Consumer<f32>,
    channels: usize,
    quantum: usize,
    primed: bool,
    shared: Arc<SharedState>,
}

impl Engine {
    fn new(
        mainloop: pw::main_loop::MainLoopRc,
        core: pw::core::CoreRc,
        registry: pw::registry::RegistryRc,
        shared: Arc<SharedState>,
        config: Config,
    ) -> Result<Self> {
        let nodes = Rc::new(RefCell::new(HashMap::new()));
        let ports = Rc::new(RefCell::new(HashMap::new()));
        let dirty = Rc::new(Cell::new(false));

        let nodes_reg = nodes.clone();
        let ports_reg = ports.clone();
        let dirty_reg = dirty.clone();
        let registry_listener = registry
            .add_listener_local()
            .global(move |global| {
                let Some(props) = global.props else { return };
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        let name = props.get("node.name").unwrap_or("").to_string();
                        let media_class = props.get("media.class").unwrap_or("").to_string();
                        let description = props
                            .get("node.description")
                            .or_else(|| props.get("node.nick"))
                            .or_else(|| props.get("device.description"))
                            .unwrap_or(&name)
                            .to_string();
                        if name.is_empty() {
                            return;
                        }
                        nodes_reg.borrow_mut().insert(
                            global.id,
                            NodeMeta {
                                id: global.id,
                                name,
                                description,
                                media_class,
                            },
                        );
                        dirty_reg.set(true);
                    }
                    pw::types::ObjectType::Port => {
                        let node_id = props
                            .get("node.id")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        if node_id == 0 {
                            return;
                        }
                        ports_reg.borrow_mut().insert(
                            global.id,
                            PortMeta {
                                id: global.id,
                                node_id,
                                name: props.get("port.name").unwrap_or("").to_string(),
                                direction: props.get("port.direction").unwrap_or("").to_string(),
                                channel: props.get("audio.channel").map(str::to_string),
                            },
                        );
                    }
                    _ => {}
                }
            })
            .global_remove({
                let nodes = nodes.clone();
                let ports = ports.clone();
                let dirty = dirty.clone();
                move |id| {
                    nodes.borrow_mut().remove(&id);
                    ports.borrow_mut().remove(&id);
                    dirty.set(true);
                }
            })
            .register();

        // Let the registry deliver its initial globals before we create nodes.
        roundtrip(&mainloop, &core)?;

        let (virtual_node, virtual_listener, virtual_node_id) = create_virtual_source(
            &mainloop,
            &core,
            &registry,
            &nodes,
            &config.audio.virtual_source_name,
            &config.audio.virtual_source_description,
            config.audio.quantum,
            config.audio.lock_quantum,
        )?;

        let (mut presets, presets_corrupt) = PresetStore::load_with_status();
        presets.ensure_builtins();
        let active_preset = presets
            .active(&config.presets.active)
            .cloned()
            .unwrap_or_else(|| presets.presets[0].clone());
        if !presets_corrupt {
            let _ = presets.save();
        } else {
            shared.set_error(Some(
                "presets.toml corrupto: se usaron presets por defecto (respaldo en presets.toml.bak)"
                    .into(),
            ));
        }

        let master_gain_db = config.audio.gain_db;
        let preset_gain_db = active_preset.gain_db();
        let total_gain_db = master_gain_db + preset_gain_db;
        let gain = Arc::new(AtomicU32::new(total_gain_db.to_bits()));
        let mute = Arc::new(AtomicBool::new(config.audio.mute));
        let pitch_control = Arc::new(AtomicU32::new(active_preset.pitch_semitones.to_bits()));
        let formant_control = Arc::new(AtomicU32::new(active_preset.formant_semitones.to_bits()));
        let noise_strength = Arc::new(AtomicU32::new(
            active_preset
                .noise_reduction()
                .unwrap_or(mvx_core::presets::DEFAULT_NOISE_STRENGTH)
                .to_bits(),
        ));
        let noise_enabled = Arc::new(AtomicBool::new(active_preset.noise_reduction().is_some()));
        let speaker_lookahead_ms = Arc::new(AtomicU32::new(
            active_preset
                .speaker_lock()
                .map(|l| l.lookahead_ms)
                .unwrap_or(120),
        ));
        let bypass = Arc::new(AtomicBool::new(false));

        let mut engine = Self {
            capture: None,
            playback: None,
            links: Vec::new(),
            linked_ports: HashSet::new(),
            _virtual_listener: virtual_listener,
            _virtual_node: virtual_node,
            virtual_node_id: Some(virtual_node_id),
            _registry_listener: registry_listener,
            _registry: registry,
            core,
            mainloop,
            shared,
            config,
            nodes,
            ports,
            dirty,
            selected_input: None,
            presets,
            active_preset,
            master_gain_db,
            gain,
            mute,
            pitch_control,
            formant_control,
            noise_strength,
            noise_enabled,
            speaker_lookahead_ms,
            bypass,
            speaker: None,
            last_retry: Instant::now() - Duration::from_secs(10),
            link_warned: false,
            presets_dirty: false,
            last_presets_save: Instant::now(),
        };

        engine.shared.set_presets(engine.presets.presets.clone());
        engine.shared.set_preset_state(&engine.active_preset);
        engine
            .shared
            .set_voice_profiles(mvx_core::list_voice_profiles());
        engine.shared.set_models_installed(
            mvx_core::speaker_model_path().exists() && mvx_core::vad_model_path().exists(),
        );
        engine.refresh_devices();
        engine.selected_input = engine
            .config
            .audio
            .input
            .clone()
            .filter(|name| engine.nodes.borrow().values().any(|n| &n.name == name))
            .or_else(|| engine.physical_devices().first().map(|d| d.name.clone()));

        if let Some(name) = engine.selected_input.clone() {
            let desc = engine
                .nodes
                .borrow()
                .values()
                .find(|n| n.name == name)
                .map(|n| n.description.clone());
            engine.shared.set_selected_input(Some(name), desc);
        }

        engine.rebuild_audio_path()?;
        engine.update_latency();
        Ok(engine)
    }

    fn physical_devices(&self) -> Vec<DeviceInfo> {
        let virtual_name = &self.config.audio.virtual_source_name;
        let mut devices: Vec<_> = self
            .nodes
            .borrow()
            .values()
            .filter(|n| n.media_class == "Audio/Source")
            .filter(|n| !n.name.starts_with("mvx."))
            .filter(|n| !n.name.contains(".monitor"))
            .filter(|n| !n.name.contains("echo-cancel"))
            .filter(|n| &n.name != virtual_name)
            .map(|n| DeviceInfo {
                id: n.id,
                name: n.name.clone(),
                description: n.description.clone(),
                media_class: n.media_class.clone(),
            })
            .collect();
        devices.sort_by(|a, b| {
            a.description
                .to_lowercase()
                .cmp(&b.description.to_lowercase())
        });
        devices
    }

    fn refresh_devices(&self) {
        self.shared.set_devices(self.physical_devices());
    }

    fn update_latency(&self) {
        let quantum = self.config.audio.quantum.max(1);
        let latency_ms = quantum as f32 * 1000.0 / SAMPLE_RATE as f32;
        self.shared.set_latency(quantum, latency_ms);
    }

    fn update_gain_atomic(&self) {
        let preset_gain = self.active_preset.gain_db();
        let total = (self.master_gain_db + preset_gain).clamp(-72.0, 36.0);
        self.gain.store(total.to_bits(), Ordering::Relaxed);
        self.shared
            .set_gain(self.master_gain_db, self.mute.load(Ordering::Relaxed));
    }

    fn needs_rebuild(&self, next: &Preset) -> bool {
        let old = &self.active_preset;
        old.quality != next.quality
            || old.speaker_lock().is_some() != next.speaker_lock().is_some()
            || old
                .speaker_lock()
                .zip(next.speaker_lock())
                .map(|(a, b)| a.profile != b.profile)
                .unwrap_or(false)
    }

    fn apply_preset(&mut self, mut preset: Preset, rebuild: bool) -> Result<()> {
        preset.normalize();
        let rebuild = rebuild || self.needs_rebuild(&preset);
        self.active_preset = preset.clone();
        let active_changed = self.config.presets.active != preset.id;
        self.config.presets.active = preset.id.clone();
        // Slider movements call this many times per second. Only the active
        // preset id lives in config.toml, so avoid rewriting it on every tick.
        if active_changed {
            if let Err(err) = self.config.save() {
                debug!("no se pudo guardar el preset activo: {err}");
            }
        }
        self.shared.set_preset_state(&preset);
        self.shared.set_presets(self.presets.presets.clone());
        self.pitch_control
            .store(preset.pitch_semitones.to_bits(), Ordering::Relaxed);
        self.formant_control
            .store(preset.formant_semitones.to_bits(), Ordering::Relaxed);
        self.noise_strength.store(
            preset
                .noise_reduction()
                .unwrap_or(mvx_core::presets::DEFAULT_NOISE_STRENGTH)
                .to_bits(),
            Ordering::Relaxed,
        );
        self.noise_enabled
            .store(preset.noise_reduction().is_some(), Ordering::Relaxed);
        if let Some(lock) = preset.speaker_lock() {
            self.speaker_lookahead_ms
                .store(lock.lookahead_ms, Ordering::Relaxed);
            self.shared.set_speaker_config(lock.threshold, lock.hold_ms);
        }
        self.update_gain_atomic();
        if rebuild {
            self.rebuild_audio_path()?;
        }
        Ok(())
    }

    fn mark_presets_dirty(&mut self) {
        self.presets_dirty = true;
    }

    fn flush_presets(&mut self, force: bool) {
        if !self.presets_dirty {
            return;
        }
        if !force && self.last_presets_save.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.presets_dirty = false;
        self.last_presets_save = Instant::now();
        if let Err(err) = self.presets.save() {
            self.shared
                .set_error(Some(format!("no se pudo guardar presets.toml: {err}")));
        }
    }

    fn handle_command(&mut self, command: AudioCommand) {
        match command {
            AudioCommand::SetInput(name) => {
                debug!("set input: {name}");
                self.selected_input = Some(name.clone());
                let desc = self
                    .nodes
                    .borrow()
                    .values()
                    .find(|n| n.name == name)
                    .map(|n| n.description.clone());
                self.shared.set_selected_input(Some(name.clone()), desc);
                self.config.audio.input = Some(name);
                if let Err(err) = self.config.save() {
                    debug!("no se pudo guardar la entrada en config: {err}");
                }
                self.shared.set_error(None);
                if let Err(err) = self.rebuild_audio_path() {
                    self.shared
                        .set_error(Some(format!("no se pudo cambiar de entrada: {err}")));
                }
            }
            AudioCommand::SetGain(gain_db) => {
                self.master_gain_db = gain_db.clamp(-60.0, 24.0);
                self.config.audio.gain_db = self.master_gain_db;
                self.update_gain_atomic();
                let _ = self.config.save();
            }
            AudioCommand::SetMute(mute) => {
                self.mute.store(mute, Ordering::Relaxed);
                self.config.audio.mute = mute;
                let _ = self.config.save();
                self.update_gain_atomic();
            }
            AudioCommand::ApplyPreset { id } => {
                let Some(preset) = self.presets.find(&id).cloned() else {
                    self.shared
                        .set_error(Some(format!("no existe el preset {id}")));
                    return;
                };
                self.flush_presets(true);
                if let Err(err) = self.apply_preset(preset, false) {
                    self.shared
                        .set_error(Some(format!("no se pudo aplicar el preset: {err}")));
                }
            }
            AudioCommand::UpsertPreset { preset, activate } => {
                if let Err(err) = self.presets.upsert(preset.clone()) {
                    self.shared
                        .set_error(Some(format!("preset inválido: {err}")));
                    return;
                }
                self.mark_presets_dirty();
                self.shared.set_presets(self.presets.presets.clone());
                if activate {
                    let stored = self.presets.find(&preset.id).cloned().unwrap_or(preset);
                    if let Err(err) = self.apply_preset(stored, false) {
                        self.shared
                            .set_error(Some(format!("no se pudo aplicar el preset: {err}")));
                    }
                }
            }
            AudioCommand::DeletePreset { id } => match self.presets.remove(&id) {
                Ok(true) => {
                    self.mark_presets_dirty();
                    self.flush_presets(true);
                    self.shared.set_presets(self.presets.presets.clone());
                    if self.active_preset.id == id {
                        let natural = self
                            .presets
                            .find(mvx_core::PRESET_NATURAL)
                            .cloned()
                            .unwrap_or_else(|| self.presets.presets[0].clone());
                        let _ = self.apply_preset(natural, false);
                    }
                }
                Ok(false) => {}
                Err(err) => self.shared.set_error(Some(err.to_string())),
            },
            AudioCommand::ResetPreset { id } => match self.presets.reset_builtin(&id) {
                Ok(true) => {
                    self.mark_presets_dirty();
                    self.flush_presets(true);
                    self.shared.set_presets(self.presets.presets.clone());
                    if self.active_preset.id == id {
                        if let Some(preset) = self.presets.find(&id).cloned() {
                            let _ = self.apply_preset(preset, false);
                        }
                    }
                }
                Ok(false) => {}
                Err(err) => self.shared.set_error(Some(err.to_string())),
            },
            AudioCommand::EnrollVoice { name, seconds } => {
                if let Err(err) = mvx_core::validate_profile_name(&name) {
                    self.shared.set_error(Some(err));
                    return;
                }
                if let Some(speaker) = &self.speaker {
                    speaker.request_enrollment(name.clone(), seconds);
                    self.shared
                        .set_speaker_status(format!("enrolando «{name}» durante {seconds}s"));
                } else if !self.shared.models_installed() {
                    self.shared.set_speaker_status(
                        "instalá los modelos de voz antes de enrolar un perfil",
                    );
                } else if self.active_preset.speaker_lock().is_none() {
                    self.shared
                        .set_speaker_status("activá «Solo mi voz» antes de enrolar un perfil");
                } else {
                    self.shared
                        .set_speaker_status("no se pudo iniciar el analizador de voz");
                }
            }
            AudioCommand::DeleteVoice { name } => {
                if let Err(err) = mvx_core::validate_profile_name(&name) {
                    self.shared.set_error(Some(err));
                    return;
                }
                let path = mvx_core::profile_path(&name);
                match std::fs::remove_file(path) {
                    Ok(()) => {
                        self.shared
                            .set_voice_profiles(mvx_core::list_voice_profiles());
                        self.shared
                            .set_speaker_status(format!("voz {name} eliminada"));
                        let active_has_lock = self.active_preset.speaker_lock().is_some();
                        let mut changed = false;
                        if let Some(mut preset) = self.presets.find(&self.active_preset.id).cloned()
                        {
                            for filter in &mut preset.filters {
                                if let mvx_core::presets::FilterConfig::SpeakerLock {
                                    profile,
                                    ..
                                } = filter
                                {
                                    if profile == &name {
                                        profile.clear();
                                        changed = true;
                                    }
                                }
                            }
                            if changed {
                                let _ = self.presets.upsert(preset.clone());
                                self.mark_presets_dirty();
                                self.flush_presets(true);
                            }
                        }
                        // Even if the preset did not reference the profile (for
                        // example when enrollment came from a client that did
                        // not update the preset), the worker may have it loaded
                        // in memory. Restarting the speaker preset drops it.
                        if changed || active_has_lock {
                            if let Some(preset) = self.presets.find(&self.active_preset.id).cloned()
                            {
                                if let Err(err) = self.apply_preset(preset, true) {
                                    self.shared.set_error(Some(format!(
                                        "no se pudo recargar el perfil de voz: {err}"
                                    )));
                                }
                            } else if let Err(err) = self.rebuild_audio_path() {
                                self.shared.set_error(Some(format!(
                                    "no se pudo recargar el perfil de voz: {err}"
                                )));
                            }
                        }
                    }
                    Err(err) => self
                        .shared
                        .set_error(Some(format!("no se pudo borrar la voz {name}: {err}"))),
                }
            }
            AudioCommand::ReloadAudio => {
                if let Err(err) = self.rebuild_audio_path() {
                    self.shared
                        .set_error(Some(format!("no se pudo recargar el audio: {err}")));
                }
            }
            AudioCommand::Rescan => {
                self.dirty.set(true);
            }
            AudioCommand::Tick => self.tick(),
            AudioCommand::Shutdown => {
                self.flush_presets(true);
                self.shared.set_running(false);
                self.mainloop.quit();
            }
        }
    }

    fn tick(&mut self) {
        if self.dirty.replace(false) {
            self.refresh_devices();
        }
        self.flush_presets(false);

        // If the selected physical source disappeared, fall back to the first one.
        let selected_exists = self
            .selected_input
            .as_ref()
            .map(|name| self.nodes.borrow().values().any(|n| &n.name == name))
            .unwrap_or(true);
        if !selected_exists {
            let fallback = self.physical_devices().first().map(|d| d.name.clone());
            self.selected_input = fallback.clone();
            self.shared.set_selected_input(
                fallback.clone(),
                fallback.as_ref().and_then(|name| {
                    self.nodes
                        .borrow()
                        .values()
                        .find(|n| &n.name == name)
                        .map(|n| n.description.clone())
                }),
            );
            self.shared.set_error(Some(
                "la entrada seleccionada desapareció; usando el fallback".into(),
            ));
            if let Err(err) = self.rebuild_audio_path() {
                self.shared
                    .set_error(Some(format!("fallback falló: {err}")));
            }
        }

        self.try_link();

        // Retry capture creation if there is no active path and enough time has passed.
        if self.capture.is_none() && self.last_retry.elapsed() > Duration::from_secs(2) {
            if let Err(err) = self.rebuild_audio_path() {
                debug!("retry capture: {err}");
                self.last_retry = Instant::now();
            }
        }
    }

    fn rebuild_audio_path(&mut self) -> Result<()> {
        self.capture = None;
        self.playback = None;
        self.speaker = None;
        self.links.clear();
        self.linked_ports.clear();
        self.link_warned = false;
        self.shared.set_running(false);
        self.shared.set_speaker_enabled(false, false);
        self.last_retry = Instant::now();

        let speaker_producer = if let Some(lock) = self.active_preset.speaker_lock() {
            let model = self
                .config
                .dsp
                .speaker_model
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(mvx_core::speaker_model_path);
            let vad = self
                .config
                .dsp
                .vad_model
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(mvx_core::vad_model_path);
            self.shared.set_speaker_config(lock.threshold, lock.hold_ms);
            let start = SpeakerWorker::start(
                self.shared.clone(),
                lock.profile,
                model,
                vad,
                self.config.dsp.speaker_analysis_hz,
            );
            self.speaker = start.worker;
            start.producer
        } else {
            self.shared.set_speaker_enabled(false, false);
            self.shared.set_speaker_metrics(0.0, false, false);
            self.shared.set_speaker_status("desactivado");
            None
        };

        let (producer, consumer) = RingBuffer::new(RING_CAPACITY);
        self.playback = Some(self.create_playback(consumer)?);
        self.capture = Some(self.create_capture(producer, speaker_producer)?);
        Ok(())
    }

    fn capture_props(&self) -> pw::properties::PropertiesBox {
        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => "mvx.capture",
            *pw::keys::NODE_DESCRIPTION => "MVX Capture",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => format!("{}/{}", self.config.audio.quantum.max(1), SAMPLE_RATE),
        };
        if let Some(name) = &self.selected_input {
            props.insert(*pw::keys::TARGET_OBJECT, name.as_str());
        }
        props
    }

    fn create_capture(
        &self,
        producer: Producer<f32>,
        speaker: Option<Producer<f32>>,
    ) -> Result<Capture> {
        let stream =
            pw::stream::StreamRc::new(self.core.clone(), "mvx-capture", self.capture_props())
                .context("no se pudo crear el stream de captura")?;

        let lock = self.active_preset.speaker_lock();
        let dsp = DspChain::new(DspParams {
            pitch_semitones: self.active_preset.pitch_semitones,
            formant_semitones: self.active_preset.formant_semitones,
            quality: self.active_preset.quality,
            noise_reduction: self.active_preset.noise_reduction(),
            speaker_gate: lock.as_ref().map(|_| self.shared.speaker_gate()),
            speaker_lookahead_ms: self.speaker_lookahead_ms.clone(),
            gain_db: self.gain.clone(),
            mute: self.mute.clone(),
            pitch_control: self.pitch_control.clone(),
            formant_control: self.formant_control.clone(),
            noise_strength: self.noise_strength.clone(),
            noise_enabled: self.noise_enabled.clone(),
            bypass: self.bypass.clone(),
        })?;
        self.shared.set_dsp_latency(dsp.latency_ms());
        if lock.is_none() {
            self.shared.set_speaker_enabled(false, false);
            self.shared.set_speaker_status("desactivado");
        }

        let data = CaptureData {
            producer,
            channels: CHANNELS as usize,
            dsp,
            mono: vec![0.0; 8_192],
            processed: vec![0.0; 8_192],
            speaker,
            shared: self.shared.clone(),
        };

        let listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, data, _old, new| match new {
                pw::stream::StreamState::Streaming => {
                    data.shared.set_running(true);
                    data.shared.set_error(None);
                }
                pw::stream::StreamState::Error(msg) => {
                    data.shared.set_error(Some(format!("captura: {msg}")));
                }
                pw::stream::StreamState::Paused
                | pw::stream::StreamState::Unconnected
                | pw::stream::StreamState::Connecting => {
                    data.shared.set_running(false);
                }
            })
            .param_changed(|_, data, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                    return;
                };
                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    return;
                }
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    data.channels = info.channels().max(1) as usize;
                    debug!(
                        "capture format: {:?} {} Hz {} ch",
                        info.format(),
                        info.rate(),
                        info.channels()
                    );
                    data.shared.set_format(info.rate(), info.channels());
                }
            })
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    data.shared.inc_xrun();
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data_buf) = datas.first_mut() else {
                    return;
                };
                let (offset, size) = {
                    let chunk = data_buf.chunk();
                    (chunk.offset() as usize, chunk.size() as usize)
                };
                let Some(slice) = data_buf.data() else { return };
                let channels = data.channels.max(1);
                let frame_bytes = std::mem::size_of::<f32>() * channels;
                if frame_bytes == 0 {
                    return;
                }
                let end = (offset + size).min(slice.len());
                if end <= offset {
                    return;
                }
                let frames = (end - offset) / frame_bytes;
                data.shared.inc_capture_block(frames);

                let mut peak = 0.0f32;
                let mut local_clips = 0u64;
                let mut done = 0usize;
                let capacity = data.mono.len();
                'chunks: while done < frames {
                    let chunk_frames = (frames - done).min(capacity);
                    for index in 0..chunk_frames {
                        let base = offset + (done + index) * frame_bytes;
                        let mut mono = 0.0f32;
                        for ch in 0..channels {
                            let start = base + ch * 4;
                            let sample = f32::from_le_bytes([
                                slice[start],
                                slice[start + 1],
                                slice[start + 2],
                                slice[start + 3],
                            ]);
                            mono += sample;
                        }
                        mono /= channels as f32;
                        let abs = mono.abs();
                        peak = peak.max(abs);
                        if abs >= 0.999 {
                            local_clips += 1;
                        }
                        data.mono[index] = mono;
                        if let Some(speaker) = data.speaker.as_mut() {
                            let _ = speaker.push(mono);
                        }
                    }

                    data.dsp.process(
                        &data.mono[..chunk_frames],
                        &mut data.processed[..chunk_frames],
                    );

                    for sample in &data.processed[..chunk_frames] {
                        if data.producer.push(*sample).is_err() {
                            data.shared.inc_overflow();
                            break 'chunks;
                        }
                    }
                    done += chunk_frames;
                }
                if local_clips > 0 {
                    data.shared.add_input_clip(local_clips);
                }
                data.shared.set_peak(peak);
            })
            .register()
            .context("no se pudo registrar el listener de captura")?;

        connect_audio_stream(
            &stream,
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
        )?;

        Ok(Capture {
            _listener: listener,
            _stream: stream,
        })
    }

    fn create_playback(&self, consumer: Consumer<f32>) -> Result<Playback> {
        let props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => "mvx.output",
            *pw::keys::NODE_DESCRIPTION => "MVX Output",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => format!("{}/{}", self.config.audio.quantum.max(1), SAMPLE_RATE),
            *pw::keys::TARGET_OBJECT => self.config.audio.virtual_source_name.as_str(),
        };

        let stream = pw::stream::StreamRc::new(self.core.clone(), "mvx-output", props)
            .context("no se pudo crear el stream de playback")?;
        let data = PlaybackData {
            consumer,
            channels: CHANNELS as usize,
            quantum: self.config.audio.quantum.max(1) as usize,
            primed: false,
            shared: self.shared.clone(),
        };

        let listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, data, _old, new| match new {
                pw::stream::StreamState::Streaming => {
                    data.shared.set_error(None);
                }
                pw::stream::StreamState::Error(msg) => {
                    data.shared.set_error(Some(format!("playback: {msg}")));
                }
                _ => {}
            })
            .param_changed(|_, data, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                    return;
                };
                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    return;
                }
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    data.channels = info.channels().max(1) as usize;
                    debug!(
                        "playback format: {:?} {} Hz {} ch",
                        info.format(),
                        info.rate(),
                        info.channels()
                    );
                }
            })
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    data.shared.inc_xrun();
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data_buf) = datas.first_mut() else {
                    return;
                };
                let channels = data.channels.max(1);
                let stride = std::mem::size_of::<f32>() * channels;
                let Some(slice) = data_buf.data() else { return };
                // The capture side may fill the ring before the first playback
                // callback runs. Keep only a small, bounded pre-roll so the
                // bridge starts at low latency instead of inheriting the whole
                // ring as latency. The ring buffer stores mono frames.
                let target = 2 * data.quantum;
                let max_prefill = 4 * data.quantum;
                let slots = data.consumer.slots();
                if !data.primed {
                    if slots < target {
                        // Wait until buffer is primed with initial target frames
                        let frames = slice.len() / stride;
                        data.shared.inc_playback_block(frames);
                        for b in slice.iter_mut() {
                            *b = 0;
                        }
                        return;
                    }
                    data.primed = true;
                } else if slots > max_prefill {
                    let excess = slots.saturating_sub(target);
                    for _ in 0..excess {
                        let _ = data.consumer.pop();
                    }
                }
                let frames = slice.len() / stride;
                data.shared.inc_playback_block(frames);
                data.shared.set_ring_fill(data.consumer.slots());
                let mut underrun = false;
                let mut peak = 0.0f32;
                for frame in 0..frames {
                    let base = frame * stride;
                    let sample = match data.consumer.pop() {
                        Ok(v) => v,
                        Err(_) => {
                            underrun = true;
                            0.0
                        }
                    };
                    peak = peak.max(sample.abs());
                    let bytes = sample.to_le_bytes();
                    for ch in 0..channels {
                        let start = base + ch * 4;
                        slice[start..start + 4].copy_from_slice(&bytes);
                    }
                }
                if underrun {
                    data.shared.inc_underrun();
                }
                data.shared.set_output_peak(peak);
                let chunk = data_buf.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * frames) as _;
            })
            .register()
            .context("no se pudo registrar el listener de playback")?;

        connect_audio_stream(
            &stream,
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        )?;

        let node_id = stream.node_id();
        Ok(Playback {
            _listener: listener,
            _stream: stream,
            node_id,
        })
    }

    fn try_link(&mut self) {
        let Some(virtual_id) = self.virtual_node_id else {
            return;
        };
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // `pw_stream_get_node_id` returns PW_ID_ANY for a stream that is not
        // bound yet, so prefer the registry's global node id.
        let playback_id = self
            .nodes
            .borrow()
            .values()
            .find(|n| n.name == "mvx.output")
            .map(|n| n.id)
            .unwrap_or(playback.node_id);
        if playback_id == 0 || playback_id == u32::MAX {
            return;
        }

        let (outs, ins): (Vec<_>, Vec<_>) = {
            let ports = self.ports.borrow();
            let outs = ports
                .values()
                .filter(|p| p.node_id == playback_id && p.direction == "out")
                .cloned()
                .collect::<Vec<_>>();
            let ins = ports
                .values()
                .filter(|p| p.node_id == virtual_id && p.direction == "in")
                .cloned()
                .collect::<Vec<_>>();
            (outs, ins)
        };
        if outs.is_empty() || ins.is_empty() {
            return;
        }

        for out in &outs {
            let candidate = ins
                .iter()
                .find(|p| p.channel.is_some() && p.channel == out.channel)
                .or_else(|| ins.first());
            let Some(input) = candidate else { continue };
            if !self.linked_ports.insert((out.id, input.id)) {
                continue;
            }
            let props = properties! {
                "link.output.node" => out.node_id.to_string(),
                "link.output.port" => out.id.to_string(),
                "link.input.node" => input.node_id.to_string(),
                "link.input.port" => input.id.to_string(),
            };
            match self
                .core
                .create_object::<pw::link::Link>("link-factory", &props)
            {
                Ok(link) => {
                    self.links.push(link);
                    debug!(
                        "linked {}:{} -> {}:{}",
                        out.node_id, out.name, input.node_id, input.name
                    );
                }
                Err(err) => {
                    self.linked_ports.remove(&(out.id, input.id));
                    if !self.link_warned {
                        warn!("no se pudo crear el link de playback: {err}");
                        self.link_warned = true;
                    }
                }
            }
        }
    }
}

fn connect_audio_stream(
    stream: &pw::stream::StreamRc,
    direction: spa::utils::Direction,
    target: Option<u32>,
    flags: pw::stream::StreamFlags,
) -> Result<()> {
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);

    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| anyhow!("serializando formato de audio: {e}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).expect("POD generado inválido")];
    stream
        .connect(direction, target, flags, &mut params)
        .context("no se pudo conectar el stream de audio")?;
    Ok(())
}

fn create_virtual_source(
    mainloop: &pw::main_loop::MainLoopRc,
    core: &pw::core::CoreRc,
    registry: &pw::registry::RegistryRc,
    nodes: &Rc<RefCell<HashMap<u32, NodeMeta>>>,
    name: &str,
    description: &str,
    quantum: u32,
    lock_quantum: bool,
) -> Result<(
    Option<pw::node::Node>,
    Option<pw::proxy::ProxyListener>,
    u32,
)> {
    // Remove a stale node from a previous crashed daemon.
    if let Some(stale) = nodes.borrow().values().find(|n| n.name == name).cloned() {
        debug!("eliminando nodo virtual previo {}", stale.id);
        let _ = registry.destroy_global(stale.id);
        roundtrip(mainloop, core)?;
    }

        let mut props = properties! {
            "factory.name" => "support.null-audio-sink",
            "node.name" => name,
            "node.description" => description,
            "media.class" => "Audio/Source/Virtual",
            "audio.position" => "FL,FR",
            "audio.channels" => "2",
        };
        if lock_quantum {
            props.insert("node.force-quantum", quantum.max(1).to_string());
            props.insert("node.lock-quantum", "true");
        } else {
            props.insert("node.latency", format!("{}/{}", quantum.max(1), SAMPLE_RATE));
        }
    let node = core
        .create_object::<pw::node::Node>("adapter", &props)
        .context("no se pudo crear el micrófono virtual (factory adapter)")?;

    let bound = Rc::new(Cell::new(0u32));
    let bound_cb = bound.clone();
    let listener = node
        .upcast_ref()
        .add_listener_local()
        .bound(move |id| bound_cb.set(id))
        .register();

    roundtrip(mainloop, core)?;

    let id = bound.get();
    if id == 0 {
        return Err(anyhow!(
            "el nodo virtual no fue publicado por PipeWire (¿existe la factory adapter?)"
        ));
    }
    debug!("micrófono virtual {} listo con id {}", name, id);
    Ok((Some(node), Some(listener), id))
}

fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) -> Result<()> {
    let done = Rc::new(Cell::new(false));
    let done_cb = done.clone();
    let ml = mainloop.clone();
    let pending = core.sync(0).context("core.sync falló")?;
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_cb.set(true);
                ml.quit();
            }
        })
        .register();

    let started = Instant::now();
    let timeout = Duration::from_secs(3);
    while !done.get() {
        if started.elapsed() > timeout {
            anyhow::bail!("tiempo de espera agotado comunicándose con PipeWire (¿está corriendo el servidor?)");
        }
        mainloop.run();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_bridge_zero_underrun_under_steady_state() {
        let (mut producer, mut consumer) = RingBuffer::<f32>::new(RING_CAPACITY);
        let block_size = 256;
        let priming_target = block_size * 2;
        let mut primed = false;
        let mut underruns = 0usize;
        let mut drops = 0usize;

        let num_blocks = 100;
        let mut captured_total = 0;
        let mut played_total = 0;

        for block_idx in 0..num_blocks {
            // 1. Capture callback: receives mono samples and writes to ring
            let cap_chunk: Vec<f32> = (0..block_size)
                .map(|i| ((block_idx * block_size + i) as f32).sin())
                .collect();

            let free = producer.slots();
            if free >= cap_chunk.len() {
                for &s in &cap_chunk {
                    let _ = producer.push(s);
                }
                captured_total += cap_chunk.len();
            } else {
                drops += 1;
            }

            // 2. Playback callback: reads mono samples and expands to stereo
            let available = consumer.slots();
            let mut out_stereo = vec![0.0f32; block_size * 2]; // 2 channels

            if !primed {
                if available >= priming_target {
                    primed = true;
                } else {
                    // Pre-roll silence
                    continue;
                }
            }

            if available < block_size {
                underruns += 1;
            } else {
                for frame_idx in 0..block_size {
                    if let Ok(sample) = consumer.pop() {
                        out_stereo[frame_idx * 2] = sample;
                        out_stereo[frame_idx * 2 + 1] = sample;
                        played_total += 1;
                    }
                }
            }
        }

        assert_eq!(drops, 0, "no drops should occur with steady rate and sufficient capacity");
        assert_eq!(underruns, 0, "no underruns should occur after priming in steady state");
        assert!(primed, "bridge should have primed successfully");
        assert!(captured_total > 0, "audio should have been captured");
        assert!(played_total > 0, "audio should have been played");
    }
}

