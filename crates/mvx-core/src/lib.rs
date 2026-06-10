//! Shared state, configuration and types used by the MVX daemon, audio engine and clients.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: u32 = 1;
pub const SOCKET_NAME: &str = "mvxd.sock";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub media_class: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub daemon_version: String,
    pub protocol_version: u32,
    pub running: bool,
    pub selected_input: Option<String>,
    pub selected_input_description: Option<String>,
    pub virtual_source: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub quantum: u32,
    pub latency_ms: f32,
    pub input_peak_db: f32,
    pub output_peak_db: f32,
    pub xruns: u64,
    pub underruns: u64,
    pub overflows: u64,
    pub capture_blocks: u64,
    pub playback_blocks: u64,
    pub capture_frames: u64,
    pub playback_frames: u64,
    pub ring_fill: u64,
    pub gain_db: f32,
    pub muted: bool,
    pub devices: Vec<DeviceInfo>,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn initial() -> Self {
        Self {
            daemon_version: VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
            virtual_source: "mvx_mic".to_string(),
            sample_rate: 48_000,
            channels: 2,
            quantum: 256,
            latency_ms: 0.0,
            input_peak_db: -120.0,
            output_peak_db: -120.0,
            ..Self::default()
        }
    }
}

/// State shared between the PipeWire/audio thread and the IPC thread.
///
/// The audio callback only touches the atomics. The `Snapshot` is updated from
/// the PipeWire main loop thread and from the IPC thread, never from the RT
/// process callback.
pub struct SharedState {
    inner: RwLock<Snapshot>,
    input_peak: AtomicU32,
    output_peak: AtomicU32,
    running: AtomicBool,
    xruns: AtomicU64,
    underruns: AtomicU64,
    overflows: AtomicU64,
    capture_blocks: AtomicU64,
    playback_blocks: AtomicU64,
    capture_frames: AtomicU64,
    playback_frames: AtomicU64,
    ring_fill: AtomicU64,
}

impl SharedState {
    pub fn new(config: &Config) -> Self {
        let mut snap = Snapshot::initial();
        snap.quantum = config.audio.quantum;
        snap.sample_rate = config.audio.sample_rate;
        snap.virtual_source = config.audio.virtual_source_name.clone();
        snap.gain_db = config.audio.gain_db;
        snap.muted = config.audio.mute;
        snap.selected_input = config.audio.input.clone();
        Self {
            inner: RwLock::new(snap),
            input_peak: AtomicU32::new(0.0f32.to_bits()),
            output_peak: AtomicU32::new(0.0f32.to_bits()),
            running: AtomicBool::new(false),
            xruns: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            capture_blocks: AtomicU64::new(0),
            playback_blocks: AtomicU64::new(0),
            capture_frames: AtomicU64::new(0),
            playback_frames: AtomicU64::new(0),
            ring_fill: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut snap = self.inner.read().expect("state poisoned").clone();
        snap.running = self.running.load(Ordering::Relaxed);
        snap.input_peak_db = linear_to_db(f32::from_bits(self.input_peak.load(Ordering::Relaxed)));
        snap.output_peak_db =
            linear_to_db(f32::from_bits(self.output_peak.load(Ordering::Relaxed)));
        snap.xruns = self.xruns.load(Ordering::Relaxed);
        snap.underruns = self.underruns.load(Ordering::Relaxed);
        snap.overflows = self.overflows.load(Ordering::Relaxed);
        snap.capture_blocks = self.capture_blocks.load(Ordering::Relaxed);
        snap.playback_blocks = self.playback_blocks.load(Ordering::Relaxed);
        snap.capture_frames = self.capture_frames.load(Ordering::Relaxed);
        snap.playback_frames = self.playback_frames.load(Ordering::Relaxed);
        snap.ring_fill = self.ring_fill.load(Ordering::Relaxed);
        let channels = snap.channels.max(1) as u64;
        let rate = snap.sample_rate.max(1) as f32;
        let buffered_frames = snap.ring_fill / channels + snap.quantum as u64;
        snap.latency_ms = buffered_frames as f32 * 1000.0 / rate;
        snap
    }

    pub fn update<F: FnOnce(&mut Snapshot)>(&self, f: F) {
        let mut snap = self.inner.write().expect("state poisoned");
        f(&mut snap);
    }

    pub fn set_devices(&self, devices: Vec<DeviceInfo>) {
        self.update(|s| s.devices = devices);
    }

    pub fn set_selected_input(&self, name: Option<String>, description: Option<String>) {
        self.update(|s| {
            s.selected_input = name;
            s.selected_input_description = description;
        });
    }

    pub fn set_format(&self, sample_rate: u32, channels: u32) {
        self.update(|s| {
            s.sample_rate = sample_rate;
            s.channels = channels;
        });
    }

    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    pub fn set_error(&self, error: Option<String>) {
        let has_error = error.is_some();
        self.update(|s| s.error = error);
        if has_error {
            self.running.store(false, Ordering::Relaxed);
        }
    }

    pub fn set_gain(&self, gain_db: f32, muted: bool) {
        self.update(|s| {
            s.gain_db = gain_db;
            s.muted = muted;
        });
    }

    pub fn set_latency(&self, quantum: u32, latency_ms: f32) {
        self.update(|s| {
            s.quantum = quantum;
            s.latency_ms = latency_ms;
        });
    }

    pub fn set_peak(&self, input: f32) {
        self.input_peak.store(input.to_bits(), Ordering::Relaxed);
    }

    pub fn set_output_peak(&self, output: f32) {
        self.output_peak.store(output.to_bits(), Ordering::Relaxed);
    }

    pub fn inc_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_overflow(&self) {
        self.overflows.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_capture_block(&self, frames: usize) {
        self.capture_blocks.fetch_add(1, Ordering::Relaxed);
        self.capture_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub fn inc_playback_block(&self, frames: usize) {
        self.playback_blocks.fetch_add(1, Ordering::Relaxed);
        self.playback_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub fn set_ring_fill(&self, samples: usize) {
        self.ring_fill.store(samples as u64, Ordering::Relaxed);
    }
}

pub fn linear_to_db(v: f32) -> f32 {
    if v <= 1.0e-9 {
        -120.0
    } else {
        (20.0 * v.log10()).clamp(-120.0, 12.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub audio: AudioConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audio: AudioConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Stable PipeWire node.name of the physical input. `None` means "first available".
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_quantum")]
    pub quantum: u32,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default = "default_virtual_name")]
    pub virtual_source_name: String,
    #[serde(default = "default_virtual_desc")]
    pub virtual_source_description: String,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input: None,
            sample_rate: 48_000,
            quantum: 256,
            gain_db: 0.0,
            mute: false,
            virtual_source_name: default_virtual_name(),
            virtual_source_description: default_virtual_desc(),
        }
    }
}

fn default_sample_rate() -> u32 {
    48_000
}
fn default_quantum() -> u32 {
    256
}
fn default_virtual_name() -> String {
    "mvx_mic".to_string()
}
fn default_virtual_desc() -> String {
    "MVX Microphone".to_string()
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("mvx: no se pudo parsear {}: {err}", path.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "mvx")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".").join(".config/mvx"))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("mvx")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join(SOCKET_NAME)
}

pub fn ensure_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "mvx")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".").join(".local/share/mvx"))
}

pub fn state_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "mvx")
        .and_then(|d| d.state_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from(".").join(".local/state/mvx"))
}

pub fn maybe_remove_stale_socket(path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}
