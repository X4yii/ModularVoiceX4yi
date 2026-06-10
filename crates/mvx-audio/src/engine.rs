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

use mvx_core::{Config, DeviceInfo, SharedState};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const RING_CAPACITY: usize = 16_384;
const TICK: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub enum AudioCommand {
    SetInput(String),
    SetGain(f32),
    SetMute(bool),
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
    gain: Arc<AtomicU32>,
    mute: Arc<AtomicBool>,
    last_retry: Instant,
    link_warned: bool,
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
    gain: Arc<AtomicU32>,
    mute: Arc<AtomicBool>,
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
        )?;

        let gain_db = config.audio.gain_db;
        let gain = Arc::new(AtomicU32::new(gain_db.to_bits()));
        let mute = Arc::new(AtomicBool::new(config.audio.mute));

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
            gain,
            mute,
            last_retry: Instant::now() - Duration::from_secs(10),
            link_warned: false,
        };

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
        let mut devices: Vec<_> = self
            .nodes
            .borrow()
            .values()
            .filter(|n| n.media_class == "Audio/Source")
            .filter(|n| !n.name.starts_with("mvx."))
            .filter(|n| !n.name.contains(".monitor"))
            .filter(|n| !n.name.contains("echo-cancel"))
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
                let gain_db = gain_db.clamp(-60.0, 24.0);
                self.gain.store(gain_db.to_bits(), Ordering::Relaxed);
                self.config.audio.gain_db = gain_db;
                let _ = self.config.save();
                self.shared
                    .set_gain(gain_db, self.mute.load(Ordering::Relaxed));
            }
            AudioCommand::SetMute(mute) => {
                self.mute.store(mute, Ordering::Relaxed);
                self.config.audio.mute = mute;
                let _ = self.config.save();
                self.shared
                    .set_gain(f32::from_bits(self.gain.load(Ordering::Relaxed)), mute);
            }
            AudioCommand::Rescan => {
                self.dirty.set(true);
            }
            AudioCommand::Tick => self.tick(),
            AudioCommand::Shutdown => {
                self.shared.set_running(false);
                self.mainloop.quit();
            }
        }
    }

    fn tick(&mut self) {
        if self.dirty.replace(false) {
            self.refresh_devices();
        }

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
        self.links.clear();
        self.linked_ports.clear();
        self.link_warned = false;
        self.shared.set_running(false);
        self.last_retry = Instant::now();

        let (producer, consumer) = RingBuffer::new(RING_CAPACITY);
        self.playback = Some(self.create_playback(consumer)?);
        self.capture = Some(self.create_capture(producer)?);
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

    fn create_capture(&self, producer: Producer<f32>) -> Result<Capture> {
        let stream =
            pw::stream::StreamRc::new(self.core.clone(), "mvx-capture", self.capture_props())
                .context("no se pudo crear el stream de captura")?;

        let data = CaptureData {
            producer,
            channels: CHANNELS as usize,
            gain: self.gain.clone(),
            mute: self.mute.clone(),
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
                let gain_db = f32::from_bits(data.gain.load(Ordering::Relaxed));
                let gain = if data.mute.load(Ordering::Relaxed) {
                    0.0
                } else {
                    10f32.powf(gain_db / 20.0)
                };
                let mut peak = 0.0f32;
                for frame in 0..frames {
                    let base = offset + frame * frame_bytes;
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
                    mono = (mono / channels as f32) * gain;
                    peak = peak.max(mono.abs());
                    if data.producer.push(mono).is_err() {
                        data.shared.inc_overflow();
                        break;
                    }
                    if data.producer.push(mono).is_err() {
                        data.shared.inc_overflow();
                        break;
                    }
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
                // ring as latency.
                let target = 2 * data.quantum * channels;
                let max_prefill = 4 * data.quantum * channels;
                let slots = data.consumer.slots();
                if !data.primed || slots > max_prefill {
                    let excess = slots.saturating_sub(target);
                    for _ in 0..excess {
                        let _ = data.consumer.pop();
                    }
                    data.primed = true;
                }
                let frames = (slice.len() / stride).min(data.quantum);
                data.shared.inc_playback_block(frames);
                data.shared.set_ring_fill(data.consumer.slots());
                let mut underrun = false;
                let mut peak = 0.0f32;
                for frame in 0..frames {
                    let base = frame * stride;
                    for ch in 0..channels {
                        let sample = match data.consumer.pop() {
                            Ok(v) => v,
                            Err(_) => {
                                underrun = true;
                                0.0
                            }
                        };
                        peak = peak.max(sample.abs());
                        let start = base + ch * 4;
                        slice[start..start + 4].copy_from_slice(&sample.to_le_bytes());
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
                .or_else(|| ins.get(0));
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

    let props = properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => name,
        "node.description" => description,
        "media.class" => "Audio/Source/Virtual",
        "audio.position" => "FL,FR",
        "audio.channels" => "2",
        "node.force-quantum" => quantum.max(1).to_string(),
        "node.lock-quantum" => "true",
    };
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
    while !done.get() {
        mainloop.run();
    }
    Ok(())
}
