//! Shared state, configuration and types used by the MVX daemon, audio engine and clients.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

pub mod presets;

pub use presets::{
    builtin_presets, FilterConfig, Preset, PresetStore, SpeakerLockConfig, PRESETS_VERSION,
    PRESET_ARDILLA, PRESET_GRAVE, PRESET_NATURAL,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: u32 = 2;
pub const CONFIG_VERSION: u32 = 2;
pub const SOCKET_NAME: &str = "mvxd.sock";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Ultra,
    #[default]
    Balanced,
    High,
}

impl Quality {
    pub fn block_size(self) -> usize {
        match self {
            // The old 512-sample option was too short for clean voice: it
            // produced audible phasiness and warbling. Keep this mode for
            // low-latency monitoring, but give it enough analysis window to
            // stay intelligible.
            Self::Ultra => 1024,
            Self::Balanced => 2048,
            Self::High => 4096,
        }
    }

    pub fn interval(self) -> usize {
        self.block_size() / 4
    }

    pub fn latency_ms(self) -> f32 {
        self.block_size() as f32 * 1000.0 / 48_000.0
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ultra => "Ultra",
            Self::Balanced => "Equilibrado",
            Self::High => "Alta",
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label.to_lowercase().as_str() {
            "balanced" | "equilibrado" => Self::Balanced,
            "high" | "alta" | "calidad" => Self::High,
            _ => Self::Ultra,
        }
    }
}

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
    pub dsp_latency_ms: f32,
    pub total_latency_ms: f32,
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
    pub preset_gain_db: f32,
    pub muted: bool,
    pub active_preset: String,
    pub active_preset_name: String,
    pub presets: Vec<Preset>,
    pub pitch_semitones: f32,
    pub formant_semitones: f32,
    pub quality: Quality,
    pub noise_reduction_strength: Option<f32>,
    pub speaker_lock_enabled: bool,
    pub speaker_lock_available: bool,
    pub speaker_lock_profile: String,
    pub speaker_score: f32,
    pub speaker_target: bool,
    pub speaker_vad: bool,
    pub speaker_status: String,
    pub voice_profiles: Vec<String>,
    pub models_installed: bool,
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
            dsp_latency_ms: 0.0,
            total_latency_ms: 0.0,
            input_peak_db: -120.0,
            output_peak_db: -120.0,
            gain_db: 0.0,
            preset_gain_db: 0.0,
            active_preset: PRESET_NATURAL.to_string(),
            active_preset_name: "Natural".to_string(),
            quality: Quality::Ultra,
            voice_profiles: Vec::new(),
            speaker_status: "desactivado".to_string(),
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
    speaker_vad: AtomicBool,
    speaker_threshold: AtomicU32,
    speaker_hold_ms: AtomicU32,
    speaker_gate: SpeakerGate,
}

/// Atomics shared with the audio callback for the speaker gate.
pub struct SpeakerGate {
    pub enabled: Arc<AtomicBool>,
    pub available: Arc<AtomicBool>,
    pub target: Arc<AtomicBool>,
    pub score: Arc<AtomicU32>,
}

impl SpeakerGate {
    pub fn new() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            available: Arc::new(AtomicBool::new(false)),
            target: Arc::new(AtomicBool::new(false)),
            score: Arc::new(AtomicU32::new(0.0f32.to_bits())),
        }
    }
}

impl Default for SpeakerGate {
    fn default() -> Self {
        Self::new()
    }
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
        snap.active_preset = config.presets.active.clone();
        snap.quality = config.dsp.default_quality;
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
            speaker_vad: AtomicBool::new(false),
            speaker_threshold: AtomicU32::new(0.72f32.to_bits()),
            speaker_hold_ms: AtomicU32::new(300),
            speaker_gate: SpeakerGate::new(),
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
        snap.speaker_lock_available = self.speaker_gate.available.load(Ordering::Relaxed);
        snap.speaker_target = self.speaker_gate.target.load(Ordering::Relaxed);
        snap.speaker_vad = self.speaker_vad.load(Ordering::Relaxed);
        snap.speaker_score = f32::from_bits(self.speaker_gate.score.load(Ordering::Relaxed));
        let channels = snap.channels.max(1) as u64;
        let rate = snap.sample_rate.max(1) as f32;
        let buffered_frames = snap.ring_fill / channels + snap.quantum as u64;
        snap.latency_ms = buffered_frames as f32 * 1000.0 / rate;
        snap.total_latency_ms = snap.latency_ms + snap.dsp_latency_ms;
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
        self.update(|s| s.error = error);
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
            s.total_latency_ms = latency_ms + s.dsp_latency_ms;
        });
    }

    pub fn set_dsp_latency(&self, latency_ms: f32) {
        self.update(|s| {
            s.dsp_latency_ms = latency_ms;
            s.total_latency_ms = s.latency_ms + latency_ms;
        });
    }

    pub fn set_preset_state(&self, preset: &Preset) {
        self.update(|s| {
            s.active_preset = preset.id.clone();
            s.active_preset_name = preset.name.clone();
            s.pitch_semitones = preset.pitch_semitones;
            s.formant_semitones = preset.formant_semitones;
            s.quality = preset.quality;
            s.preset_gain_db = preset.gain_db();
            s.noise_reduction_strength = preset.noise_reduction();
            s.speaker_lock_enabled = preset.speaker_lock().is_some();
            s.speaker_lock_profile = preset
                .speaker_lock()
                .map(|lock| lock.profile)
                .unwrap_or_default();
        });
    }

    pub fn set_presets(&self, presets: Vec<Preset>) {
        self.update(|s| s.presets = presets);
    }

    pub fn set_active_preset_id(&self, id: String) {
        self.update(|s| s.active_preset = id);
    }

    pub fn set_voice_profiles(&self, profiles: Vec<String>) {
        self.update(|s| s.voice_profiles = profiles);
    }

    pub fn set_models_installed(&self, installed: bool) {
        self.update(|s| s.models_installed = installed);
    }

    pub fn models_installed(&self) -> bool {
        self.inner.read().expect("state poisoned").models_installed
    }

    pub fn set_speaker_status(&self, status: impl Into<String>) {
        self.update(|s| s.speaker_status = status.into());
    }

    pub fn set_speaker_metrics(&self, score: f32, target: bool, vad: bool) {
        self.speaker_gate
            .score
            .store(score.to_bits(), Ordering::Relaxed);
        self.speaker_gate.target.store(target, Ordering::Relaxed);
        self.speaker_vad.store(vad, Ordering::Relaxed);
    }

    pub fn set_speaker_config(&self, threshold: f32, hold_ms: u32) {
        self.speaker_threshold
            .store(threshold.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.speaker_hold_ms
            .store(hold_ms.min(5_000), Ordering::Relaxed);
    }

    pub fn speaker_config(&self) -> (f32, u32) {
        (
            f32::from_bits(self.speaker_threshold.load(Ordering::Relaxed)),
            self.speaker_hold_ms.load(Ordering::Relaxed),
        )
    }

    pub fn set_speaker_enabled(&self, enabled: bool, available: bool) {
        self.speaker_gate
            .enabled
            .store(enabled && available, Ordering::Relaxed);
        self.speaker_gate
            .available
            .store(available, Ordering::Relaxed);
    }

    pub fn speaker_gate(&self) -> SpeakerGate {
        SpeakerGate {
            enabled: self.speaker_gate.enabled.clone(),
            available: self.speaker_gate.available.clone(),
            target: self.speaker_gate.target.clone(),
            score: self.speaker_gate.score.clone(),
        }
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
    pub config_version: u32,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub dsp: DspConfig,
    #[serde(default)]
    pub presets: PresetConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            audio: AudioConfig::default(),
            dsp: DspConfig::default(),
            presets: PresetConfig::default(),
            ui: UiConfig::default(),
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
    /// Master trim, applied after the preset gain.
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DspConfig {
    #[serde(default)]
    pub default_quality: Quality,
    /// Optional override for the speaker embedding ONNX model.
    #[serde(default)]
    pub speaker_model: Option<String>,
    /// Optional override for the Silero VAD ONNX model.
    #[serde(default)]
    pub vad_model: Option<String>,
    #[serde(default = "default_analysis_hz")]
    pub speaker_analysis_hz: u32,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            default_quality: Quality::Balanced,
            speaker_model: None,
            vad_model: None,
            speaker_analysis_hz: 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresetConfig {
    #[serde(default = "default_active_preset")]
    pub active: String,
}

impl Default for PresetConfig {
    fn default() -> Self {
        Self {
            active: default_active_preset(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default = "default_true_ui")]
    pub mouse: bool,
    #[serde(default = "default_tab")]
    pub last_tab: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            mouse: true,
            last_tab: default_tab(),
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
fn default_analysis_hz() -> u32 {
    4
}
fn default_active_preset() -> String {
    PRESET_NATURAL.to_string()
}
fn default_true_ui() -> bool {
    true
}
fn default_tab() -> String {
    "general".to_string()
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut config) => {
                    if config.config_version == 0 {
                        config.config_version = CONFIG_VERSION;
                    }
                    config
                }
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
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)?;
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

pub fn presets_path() -> PathBuf {
    config_dir().join("presets.toml")
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

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn speaker_model_path() -> PathBuf {
    models_dir().join("3dspeaker_campplus_zh_en.onnx")
}

pub fn vad_model_path() -> PathBuf {
    models_dir().join("silero_vad.onnx")
}

pub fn voices_dir() -> PathBuf {
    data_dir().join("voices")
}

pub fn profile_path(name: &str) -> PathBuf {
    voices_dir().join(format!("{name}.json"))
}

pub fn list_voice_profiles() -> Vec<String> {
    let mut profiles = Vec::new();
    if let Ok(entries) = fs::read_dir(voices_dir()) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(name) = name.strip_suffix(".json") {
                    profiles.push(name.to_string());
                }
            }
        }
    }
    profiles.sort();
    profiles
}

pub fn maybe_remove_stale_socket(path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}
