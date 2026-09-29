//! Voice presets and their optional filter chain.

use serde::{Deserialize, Serialize};

use crate::{presets_path, Quality};

pub const PRESETS_VERSION: u32 = 3;
pub const DEFAULT_NOISE_STRENGTH: f32 = 0.75;

pub const PRESET_NATURAL: &str = "natural";
pub const PRESET_BROADCAST: &str = "broadcast";
pub const PRESET_GRAVE: &str = "voz_grave";
pub const PRESET_ARDILLA: &str = "voz_ardilla";
pub const PRESET_TITAN: &str = "titan";
pub const PRESET_CYBORG: &str = "cyborg";
pub const PRESET_POLICE: &str = "police_radio";
pub const PRESET_MEGAPHONE: &str = "megaphone";
pub const PRESET_CAVE: &str = "cave_echo";
pub const PRESET_ANONYMOUS: &str = "anonymous";
pub const PRESET_ALIEN: &str = "alien";
pub const PRESET_TELEPHONE: &str = "telephone";
pub const PRESET_HELIUM: &str = "helium";
pub const PRESET_RADIO_DEMON: &str = "radio_demon";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FilterConfig {
    NoiseReduction {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_strength")]
        strength: f32,
    },
    Gain {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        gain_db: f32,
    },
    SpeakerLock {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        profile: String,
        #[serde(default = "default_threshold")]
        threshold: f32,
        #[serde(default = "default_hold_ms")]
        hold_ms: u32,
        #[serde(default = "default_lookahead_ms")]
        lookahead_ms: u32,
    },
    NoiseGate {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_gate_threshold")]
        threshold_db: f32,
        #[serde(default = "default_gate_attack_ms")]
        attack_ms: f32,
        #[serde(default = "default_gate_release_ms")]
        release_ms: f32,
    },
    HighPass {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_hpf_cutoff")]
        cutoff_hz: f32,
    },
    Equalizer {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        low_db: f32,
        #[serde(default)]
        mid_db: f32,
        #[serde(default)]
        high_db: f32,
    },
    Compressor {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_comp_threshold")]
        threshold_db: f32,
        #[serde(default = "default_comp_ratio")]
        ratio: f32,
        #[serde(default = "default_comp_attack")]
        attack_ms: f32,
        #[serde(default = "default_comp_release")]
        release_ms: f32,
        #[serde(default)]
        makeup_db: f32,
    },
    Chorus {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_chorus_rate")]
        rate_hz: f32,
        #[serde(default = "default_chorus_depth")]
        depth_ms: f32,
        #[serde(default = "default_chorus_mix")]
        mix: f32,
    },
    Robotizer {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_robot_freq")]
        frequency_hz: f32,
        #[serde(default = "default_half_mix")]
        mix: f32,
    },
    RingMod {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_ring_freq")]
        frequency_hz: f32,
        #[serde(default = "default_half_mix")]
        mix: f32,
    },
    Distortion {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_dist_drive")]
        drive: f32,
        #[serde(default = "default_crush_bits")]
        crush_bits: u32,
        #[serde(default = "default_dist_mix")]
        mix: f32,
    },
    Reverb {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_reverb_room")]
        room_size: f32,
        #[serde(default = "default_reverb_damp")]
        damping: f32,
        #[serde(default = "default_reverb_mix")]
        mix: f32,
    },
    Humanizer {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_human_depth")]
        depth_cents: f32,
        #[serde(default = "default_human_rate")]
        rate_hz: f32,
    },
}

impl FilterConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoiseReduction { .. } => "noise_reduction",
            Self::Gain { .. } => "gain",
            Self::SpeakerLock { .. } => "speaker_lock",
            Self::NoiseGate { .. } => "noise_gate",
            Self::HighPass { .. } => "high_pass",
            Self::Equalizer { .. } => "equalizer",
            Self::Compressor { .. } => "compressor",
            Self::Chorus { .. } => "chorus",
            Self::Robotizer { .. } => "robotizer",
            Self::RingMod { .. } => "ring_mod",
            Self::Distortion { .. } => "distortion",
            Self::Reverb { .. } => "reverb",
            Self::Humanizer { .. } => "humanizer",
        }
    }

    pub fn enabled(&self) -> bool {
        match self {
            Self::NoiseReduction { enabled, .. } => *enabled,
            Self::Gain { enabled, .. } => *enabled,
            Self::SpeakerLock { enabled, .. } => *enabled,
            Self::NoiseGate { enabled, .. } => *enabled,
            Self::HighPass { enabled, .. } => *enabled,
            Self::Equalizer { enabled, .. } => *enabled,
            Self::Compressor { enabled, .. } => *enabled,
            Self::Chorus { enabled, .. } => *enabled,
            Self::Robotizer { enabled, .. } => *enabled,
            Self::RingMod { enabled, .. } => *enabled,
            Self::Distortion { enabled, .. } => *enabled,
            Self::Reverb { enabled, .. } => *enabled,
            Self::Humanizer { enabled, .. } => *enabled,
        }
    }

    pub fn set_enabled(&mut self, value: bool) {
        match self {
            Self::NoiseReduction { enabled, .. } => *enabled = value,
            Self::Gain { enabled, .. } => *enabled = value,
            Self::SpeakerLock { enabled, .. } => *enabled = value,
            Self::NoiseGate { enabled, .. } => *enabled = value,
            Self::HighPass { enabled, .. } => *enabled = value,
            Self::Equalizer { enabled, .. } => *enabled = value,
            Self::Compressor { enabled, .. } => *enabled = value,
            Self::Chorus { enabled, .. } => *enabled = value,
            Self::Robotizer { enabled, .. } => *enabled = value,
            Self::RingMod { enabled, .. } => *enabled = value,
            Self::Distortion { enabled, .. } => *enabled = value,
            Self::Reverb { enabled, .. } => *enabled = value,
            Self::Humanizer { enabled, .. } => *enabled = value,
        }
    }

    pub fn normalize(&mut self) {
        match self {
            Self::NoiseReduction { strength, .. } => *strength = strength.clamp(0.0, 1.0),
            Self::Gain { gain_db, .. } => *gain_db = gain_db.clamp(-60.0, 24.0),
            Self::SpeakerLock {
                threshold,
                hold_ms,
                lookahead_ms,
                ..
            } => {
                *threshold = threshold.clamp(0.0, 1.0);
                *hold_ms = (*hold_ms).min(3_000);
                *lookahead_ms = (*lookahead_ms).min(500);
            }
            Self::NoiseGate {
                threshold_db,
                attack_ms,
                release_ms,
                ..
            } => {
                *threshold_db = threshold_db.clamp(-90.0, 0.0);
                *attack_ms = attack_ms.clamp(0.1, 500.0);
                *release_ms = release_ms.clamp(1.0, 2000.0);
            }
            Self::HighPass { cutoff_hz, .. } => {
                *cutoff_hz = cutoff_hz.clamp(20.0, 1000.0);
            }
            Self::Equalizer {
                low_db,
                mid_db,
                high_db,
                ..
            } => {
                *low_db = low_db.clamp(-24.0, 24.0);
                *mid_db = mid_db.clamp(-24.0, 24.0);
                *high_db = high_db.clamp(-24.0, 24.0);
            }
            Self::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_db,
                ..
            } => {
                *threshold_db = threshold_db.clamp(-60.0, 0.0);
                *ratio = ratio.clamp(1.0, 20.0);
                *attack_ms = attack_ms.clamp(0.1, 500.0);
                *release_ms = release_ms.clamp(1.0, 2000.0);
                *makeup_db = makeup_db.clamp(0.0, 30.0);
            }
            Self::Chorus {
                rate_hz,
                depth_ms,
                mix,
                ..
            } => {
                *rate_hz = rate_hz.clamp(0.1, 10.0);
                *depth_ms = depth_ms.clamp(0.1, 20.0);
                *mix = mix.clamp(0.0, 1.0);
            }
            Self::Robotizer {
                frequency_hz,
                mix,
                ..
            } => {
                *frequency_hz = frequency_hz.clamp(30.0, 1000.0);
                *mix = mix.clamp(0.0, 1.0);
            }
            Self::RingMod {
                frequency_hz,
                mix,
                ..
            } => {
                *frequency_hz = frequency_hz.clamp(10.0, 2000.0);
                *mix = mix.clamp(0.0, 1.0);
            }
            Self::Distortion {
                drive,
                crush_bits,
                mix,
                ..
            } => {
                *drive = drive.clamp(1.0, 30.0);
                *crush_bits = (*crush_bits).min(16);
                *mix = mix.clamp(0.0, 1.0);
            }
            Self::Reverb {
                room_size,
                damping,
                mix,
                ..
            } => {
                *room_size = room_size.clamp(0.0, 1.0);
                *damping = damping.clamp(0.0, 1.0);
                *mix = mix.clamp(0.0, 1.0);
            }
            Self::Humanizer {
                depth_cents,
                rate_hz,
                ..
            } => {
                *depth_cents = depth_cents.clamp(0.0, 100.0);
                *rate_hz = rate_hz.clamp(0.1, 20.0);
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub pitch_semitones: f32,
    #[serde(default)]
    pub formant_semitones: f32,
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub filters: Vec<FilterConfig>,
}

impl Preset {
    pub fn normalize(&mut self) {
        self.pitch_semitones = self.pitch_semitones.clamp(-24.0, 24.0);
        self.formant_semitones = self.formant_semitones.clamp(-12.0, 12.0);
        self.filters.iter_mut().for_each(FilterConfig::normalize);
    }

    pub fn gain_db(&self) -> f32 {
        self.filters
            .iter()
            .find_map(|filter| match filter {
                FilterConfig::Gain {
                    enabled: true,
                    gain_db,
                } => Some(*gain_db),
                _ => None,
            })
            .unwrap_or(0.0)
    }

    pub fn noise_reduction(&self) -> Option<f32> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::NoiseReduction {
                enabled: true,
                strength,
            } => Some(*strength),
            _ => None,
        })
    }

    pub fn speaker_lock(&self) -> Option<SpeakerLockConfig> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::SpeakerLock {
                enabled: true,
                profile,
                threshold,
                hold_ms,
                lookahead_ms,
            } => Some(SpeakerLockConfig {
                profile: profile.clone(),
                threshold: *threshold,
                hold_ms: *hold_ms,
                lookahead_ms: *lookahead_ms,
            }),
            _ => None,
        })
    }

    pub fn noise_gate(&self) -> Option<NoiseGateConfig> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::NoiseGate {
                enabled: true,
                threshold_db,
                attack_ms,
                release_ms,
            } => Some(NoiseGateConfig {
                threshold_db: *threshold_db,
                attack_ms: *attack_ms,
                release_ms: *release_ms,
            }),
            _ => None,
        })
    }

    pub fn high_pass(&self) -> Option<f32> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::HighPass {
                enabled: true,
                cutoff_hz,
            } => Some(*cutoff_hz),
            _ => None,
        })
    }

    pub fn equalizer(&self) -> Option<(f32, f32, f32)> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::Equalizer {
                enabled: true,
                low_db,
                mid_db,
                high_db,
            } => Some((*low_db, *mid_db, *high_db)),
            _ => None,
        })
    }

    pub fn compressor(&self) -> Option<CompressorConfig> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::Compressor {
                enabled: true,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_db,
            } => Some(CompressorConfig {
                threshold_db: *threshold_db,
                ratio: *ratio,
                attack_ms: *attack_ms,
                release_ms: *release_ms,
                makeup_db: *makeup_db,
            }),
            _ => None,
        })
    }

    pub fn chorus(&self) -> Option<ChorusConfig> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::Chorus {
                enabled: true,
                rate_hz,
                depth_ms,
                mix,
            } => Some(ChorusConfig {
                rate_hz: *rate_hz,
                depth_ms: *depth_ms,
                mix: *mix,
            }),
            _ => None,
        })
    }

    pub fn robotizer(&self) -> Option<(f32, f32)> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::Robotizer {
                enabled: true,
                frequency_hz,
                mix,
            } => Some((*frequency_hz, *mix)),
            _ => None,
        })
    }

    pub fn ring_mod(&self) -> Option<(f32, f32)> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::RingMod {
                enabled: true,
                frequency_hz,
                mix,
            } => Some((*frequency_hz, *mix)),
            _ => None,
        })
    }

    pub fn distortion(&self) -> Option<DistortionConfig> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::Distortion {
                enabled: true,
                drive,
                crush_bits,
                mix,
            } => Some(DistortionConfig {
                drive: *drive,
                crush_bits: *crush_bits,
                mix: *mix,
            }),
            _ => None,
        })
    }

    pub fn reverb(&self) -> Option<ReverbConfig> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::Reverb {
                enabled: true,
                room_size,
                damping,
                mix,
            } => Some(ReverbConfig {
                room_size: *room_size,
                damping: *damping,
                mix: *mix,
            }),
            _ => None,
        })
    }

    pub fn humanizer(&self) -> Option<(f32, f32)> {
        self.filters.iter().find_map(|filter| match filter {
            FilterConfig::Humanizer {
                enabled: true,
                depth_cents,
                rate_hz,
            } => Some((*depth_cents, *rate_hz)),
            _ => None,
        })
    }

    pub fn has_filter(&self, kind: &str) -> bool {
        self.filters.iter().any(|filter| filter.kind() == kind)
    }

    pub fn add_filter(&mut self, kind: &str) {
        if self.has_filter(kind) {
            return;
        }
        let filter = match kind {
            "noise_reduction" => FilterConfig::NoiseReduction {
                enabled: true,
                strength: default_strength(),
            },
            "gain" => FilterConfig::Gain {
                enabled: true,
                gain_db: 0.0,
            },
            "speaker_lock" => FilterConfig::SpeakerLock {
                enabled: false,
                profile: String::new(),
                threshold: default_threshold(),
                hold_ms: default_hold_ms(),
                lookahead_ms: default_lookahead_ms(),
            },
            "noise_gate" => FilterConfig::NoiseGate {
                enabled: true,
                threshold_db: default_gate_threshold(),
                attack_ms: default_gate_attack_ms(),
                release_ms: default_gate_release_ms(),
            },
            "high_pass" => FilterConfig::HighPass {
                enabled: true,
                cutoff_hz: default_hpf_cutoff(),
            },
            "equalizer" => FilterConfig::Equalizer {
                enabled: true,
                low_db: 0.0,
                mid_db: 0.0,
                high_db: 0.0,
            },
            "compressor" => FilterConfig::Compressor {
                enabled: true,
                threshold_db: default_comp_threshold(),
                ratio: default_comp_ratio(),
                attack_ms: default_comp_attack(),
                release_ms: default_comp_release(),
                makeup_db: 3.0,
            },
            "chorus" => FilterConfig::Chorus {
                enabled: true,
                rate_hz: default_chorus_rate(),
                depth_ms: default_chorus_depth(),
                mix: default_chorus_mix(),
            },
            "robotizer" => FilterConfig::Robotizer {
                enabled: true,
                frequency_hz: default_robot_freq(),
                mix: default_half_mix(),
            },
            "ring_mod" => FilterConfig::RingMod {
                enabled: true,
                frequency_hz: default_ring_freq(),
                mix: default_half_mix(),
            },
            "distortion" => FilterConfig::Distortion {
                enabled: true,
                drive: default_dist_drive(),
                crush_bits: default_crush_bits(),
                mix: default_dist_mix(),
            },
            "reverb" => FilterConfig::Reverb {
                enabled: true,
                room_size: default_reverb_room(),
                damping: default_reverb_damp(),
                mix: default_reverb_mix(),
            },
            "humanizer" => FilterConfig::Humanizer {
                enabled: true,
                depth_cents: default_human_depth(),
                rate_hz: default_human_rate(),
            },
            _ => return,
        };
        self.filters.push(filter);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoiseGateConfig {
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompressorConfig {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChorusConfig {
    pub rate_hz: f32,
    pub depth_ms: f32,
    pub mix: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DistortionConfig {
    pub drive: f32,
    pub crush_bits: u32,
    pub mix: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReverbConfig {
    pub room_size: f32,
    pub damping: f32,
    pub mix: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerLockConfig {
    pub profile: String,
    pub threshold: f32,
    pub hold_ms: u32,
    pub lookahead_ms: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresetStore {
    #[serde(default = "default_presets_version")]
    pub version: u32,
    #[serde(default)]
    pub presets: Vec<Preset>,
}

impl Default for PresetStore {
    fn default() -> Self {
        Self {
            version: PRESETS_VERSION,
            presets: builtin_presets(),
        }
    }
}

impl PresetStore {
    pub fn load() -> Self {
        Self::load_with_status().0
    }

    pub fn load_with_status() -> (Self, bool) {
        let path = presets_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Self>(&text) {
                Ok(mut store) => {
                    if store.version < PRESETS_VERSION {
                        store.migrate(store.version);
                    }
                    store.version = PRESETS_VERSION;
                    store.ensure_builtins();
                    (store, false)
                }
                Err(err) => {
                    crate::backup_unreadable_file(&path);
                    eprintln!("mvx: no se pudo parsear {}: {err}", path.display());
                    (Self::default(), true)
                }
            },
            Err(_) => (Self::default(), false),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = presets_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Preset> {
        self.presets.iter_mut().find(|preset| preset.id == id)
    }

    pub fn active(&self, id: &str) -> Option<&Preset> {
        self.find(id).or_else(|| self.find(PRESET_NATURAL))
    }

    pub fn upsert(&mut self, mut preset: Preset) -> anyhow::Result<()> {
        preset.normalize();
        if preset.id.trim().is_empty() {
            anyhow::bail!("el preset necesita un id");
        }
        if preset.name.trim().is_empty() {
            anyhow::bail!("el preset necesita un nombre");
        }
        if let Some(existing) = self.find_mut(&preset.id) {
            preset.builtin = existing.builtin;
            *existing = preset;
        } else {
            preset.builtin = false;
            self.presets.push(preset);
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> anyhow::Result<bool> {
        let Some(preset) = self.find(id) else {
            return Ok(false);
        };
        if preset.builtin {
            anyhow::bail!("los presets built-in no se pueden borrar; usá Restaurar");
        }
        self.presets.retain(|preset| preset.id != id);
        Ok(true)
    }

    pub fn reset_builtin(&mut self, id: &str) -> anyhow::Result<bool> {
        let Some(default) = builtin_presets().into_iter().find(|preset| preset.id == id) else {
            return Ok(false);
        };
        match self.find_mut(id) {
            Some(existing) if existing.builtin => {
                *existing = default;
            }
            _ => {
                self.presets.push(default);
            }
        }
        Ok(true)
    }

    fn migrate(&mut self, from: u32) {
        if from >= PRESETS_VERSION {
            return;
        }

        if from < 2 {
            for preset in &mut self.presets {
                // The old quality labels used 512/1024/2048-sample windows.
                // The new labels use 1024/2048/4096, so remap existing
                // presets to keep the same effective block size and latency.
                preset.quality = match preset.quality {
                    Quality::Balanced => Quality::Ultra,
                    Quality::High => Quality::Balanced,
                    Quality::Ultra => Quality::Ultra,
                };

                // The first squirrel preset shifted the formants only half as
                // far as the pitch. That sounds like a pitch-shifted voice,
                // not like Alvin. Follow the pitch with the formants.
                if preset.id == PRESET_ARDILLA && (preset.formant_semitones - 3.0).abs() < 0.001 {
                    preset.formant_semitones = 6.0;
                }

                // Built-in modulation presets must start from a clean analysis
                // window; the old `ultra` default caused warbling and clicks.
                if preset.builtin && (preset.id == PRESET_ARDILLA || preset.id == PRESET_GRAVE) {
                    preset.quality = Quality::Balanced;
                }
            }
        }
    }

    pub fn ensure_builtins(&mut self) {
        for default in builtin_presets() {
            if !self.presets.iter().any(|preset| preset.id == default.id) {
                self.presets.push(default);
            }
        }
        // A malformed custom entry should not take down the daemon.
        let count = self.presets.len();
        for (index, preset) in self.presets.iter_mut().enumerate() {
            preset.normalize();
            if preset.id.trim().is_empty() {
                preset.id = format!("custom_{}", count + index);
            }
            if preset.name.trim().is_empty() {
                preset.name = preset.id.clone();
            }
        }
    }

    pub fn next_custom_id(&self) -> String {
        let mut n = 1;
        loop {
            let id = format!("custom_{n}");
            if self.find(&id).is_none() {
                return id;
            }
            n += 1;
        }
    }
}

pub fn builtin_presets() -> Vec<Preset> {
    vec![
        Preset {
            id: PRESET_NATURAL.into(),
            name: "Natural (Limpio)".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::Gain {
                    enabled: true,
                    gain_db: 0.0,
                },
            ],
        },
        Preset {
            id: PRESET_BROADCAST.into(),
            name: "Locutor Pro (FM)".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::HighPass {
                    enabled: true,
                    cutoff_hz: 80.0,
                },
                FilterConfig::Equalizer {
                    enabled: true,
                    low_db: 2.0,
                    mid_db: 1.5,
                    high_db: 2.5,
                },
                FilterConfig::Compressor {
                    enabled: true,
                    threshold_db: -18.0,
                    ratio: 3.5,
                    attack_ms: 15.0,
                    release_ms: 120.0,
                    makeup_db: 2.5,
                },
            ],
        },
        Preset {
            id: PRESET_GRAVE.into(),
            name: "Voz grave".into(),
            builtin: true,
            pitch_semitones: -5.0,
            formant_semitones: -2.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::Equalizer {
                    enabled: true,
                    low_db: 3.0,
                    mid_db: 0.0,
                    high_db: -1.0,
                },
                FilterConfig::Compressor {
                    enabled: true,
                    threshold_db: -16.0,
                    ratio: 4.0,
                    attack_ms: 20.0,
                    release_ms: 150.0,
                    makeup_db: 2.0,
                },
            ],
        },
        Preset {
            id: PRESET_ARDILLA.into(),
            name: "Voz de ardilla".into(),
            builtin: true,
            pitch_semitones: 6.0,
            formant_semitones: 6.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::Equalizer {
                    enabled: true,
                    low_db: -4.0,
                    mid_db: 0.0,
                    high_db: 3.0,
                },
                FilterConfig::Gain {
                    enabled: true,
                    gain_db: -3.0,
                },
            ],
        },
        Preset {
            id: PRESET_TITAN.into(),
            name: "Titán de las Profundidades".into(),
            builtin: true,
            pitch_semitones: -10.0,
            formant_semitones: -5.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::Distortion {
                    enabled: true,
                    drive: 1.8,
                    crush_bits: 0,
                    mix: 0.25,
                },
                FilterConfig::Reverb {
                    enabled: true,
                    room_size: 0.75,
                    damping: 0.3,
                    mix: 0.35,
                },
            ],
        },
        Preset {
            id: PRESET_CYBORG.into(),
            name: "Cyborg / Androide".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::Robotizer {
                    enabled: true,
                    frequency_hz: 115.0,
                    mix: 0.65,
                },
                FilterConfig::Chorus {
                    enabled: true,
                    rate_hz: 1.8,
                    depth_ms: 3.0,
                    mix: 0.4,
                },
            ],
        },
        Preset {
            id: PRESET_POLICE.into(),
            name: "Radio Policial / Walkie".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::HighPass {
                    enabled: true,
                    cutoff_hz: 400.0,
                },
                FilterConfig::Equalizer {
                    enabled: true,
                    low_db: -12.0,
                    mid_db: 6.0,
                    high_db: -8.0,
                },
                FilterConfig::Distortion {
                    enabled: true,
                    drive: 4.5,
                    crush_bits: 8,
                    mix: 0.7,
                },
                FilterConfig::NoiseGate {
                    enabled: true,
                    threshold_db: -38.0,
                    attack_ms: 2.0,
                    release_ms: 50.0,
                },
            ],
        },
        Preset {
            id: PRESET_MEGAPHONE.into(),
            name: "Megáfono".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::HighPass {
                    enabled: true,
                    cutoff_hz: 500.0,
                },
                FilterConfig::Equalizer {
                    enabled: true,
                    low_db: -15.0,
                    mid_db: 8.0,
                    high_db: -6.0,
                },
                FilterConfig::Distortion {
                    enabled: true,
                    drive: 6.0,
                    crush_bits: 0,
                    mix: 0.8,
                },
            ],
        },
        Preset {
            id: PRESET_CAVE.into(),
            name: "Cueva de Ecos".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::Reverb {
                    enabled: true,
                    room_size: 0.92,
                    damping: 0.2,
                    mix: 0.55,
                },
            ],
        },
        Preset {
            id: PRESET_ANONYMOUS.into(),
            name: "Testigo Protegido (Anónimo)".into(),
            builtin: true,
            pitch_semitones: -4.0,
            formant_semitones: -3.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::Robotizer {
                    enabled: true,
                    frequency_hz: 95.0,
                    mix: 0.35,
                },
                FilterConfig::Distortion {
                    enabled: true,
                    drive: 1.2,
                    crush_bits: 10,
                    mix: 0.3,
                },
            ],
        },
        Preset {
            id: PRESET_ALIEN.into(),
            name: "Invasor Alienígena".into(),
            builtin: true,
            pitch_semitones: 3.0,
            formant_semitones: -2.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::RingMod {
                    enabled: true,
                    frequency_hz: 55.0,
                    mix: 0.6,
                },
                FilterConfig::Chorus {
                    enabled: true,
                    rate_hz: 3.0,
                    depth_ms: 4.0,
                    mix: 0.45,
                },
            ],
        },
        Preset {
            id: PRESET_TELEPHONE.into(),
            name: "Teléfono Vintage".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::HighPass {
                    enabled: true,
                    cutoff_hz: 350.0,
                },
                FilterConfig::Equalizer {
                    enabled: true,
                    low_db: -14.0,
                    mid_db: 3.0,
                    high_db: -14.0,
                },
                FilterConfig::Distortion {
                    enabled: true,
                    drive: 2.2,
                    crush_bits: 12,
                    mix: 0.4,
                },
            ],
        },
        Preset {
            id: PRESET_HELIUM.into(),
            name: "Gas Helio".into(),
            builtin: true,
            pitch_semitones: 9.0,
            formant_semitones: 9.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::HighPass {
                    enabled: true,
                    cutoff_hz: 120.0,
                },
            ],
        },
        Preset {
            id: PRESET_RADIO_DEMON.into(),
            name: "Entidad Eléctrica".into(),
            builtin: true,
            pitch_semitones: -7.0,
            formant_semitones: -4.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::RingMod {
                    enabled: true,
                    frequency_hz: 45.0,
                    mix: 0.4,
                },
                FilterConfig::Chorus {
                    enabled: true,
                    rate_hz: 2.5,
                    depth_ms: 5.0,
                    mix: 0.5,
                },
                FilterConfig::Reverb {
                    enabled: true,
                    room_size: 0.8,
                    damping: 0.4,
                    mix: 0.4,
                },
            ],
        },
    ]
}

fn default_presets_version() -> u32 {
    PRESETS_VERSION
}

fn default_true() -> bool {
    true
}

fn default_strength() -> f32 {
    DEFAULT_NOISE_STRENGTH
}

fn default_threshold() -> f32 {
    0.72
}

fn default_hold_ms() -> u32 {
    300
}

fn default_lookahead_ms() -> u32 {
    0
}

fn default_gate_threshold() -> f32 {
    -45.0
}

fn default_gate_attack_ms() -> f32 {
    5.0
}

fn default_gate_release_ms() -> f32 {
    100.0
}

fn default_hpf_cutoff() -> f32 {
    80.0
}

fn default_comp_threshold() -> f32 {
    -18.0
}

fn default_comp_ratio() -> f32 {
    4.0
}

fn default_comp_attack() -> f32 {
    15.0
}

fn default_comp_release() -> f32 {
    120.0
}

fn default_chorus_rate() -> f32 {
    1.2
}

fn default_chorus_depth() -> f32 {
    2.5
}

fn default_chorus_mix() -> f32 {
    0.4
}

fn default_robot_freq() -> f32 {
    110.0
}

fn default_ring_freq() -> f32 {
    60.0
}

fn default_half_mix() -> f32 {
    0.5
}

fn default_dist_drive() -> f32 {
    2.5
}

fn default_crush_bits() -> u32 {
    0
}

fn default_dist_mix() -> f32 {
    0.5
}

fn default_reverb_room() -> f32 {
    0.6
}

fn default_reverb_damp() -> f32 {
    0.3
}

fn default_reverb_mix() -> f32 {
    0.35
}

fn default_human_depth() -> f32 {
    15.0
}

fn default_human_rate() -> f32 {
    4.5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: &str, builtin: bool, quality: Quality, formant_semitones: f32) -> Preset {
        Preset {
            id: id.into(),
            name: id.into(),
            builtin,
            pitch_semitones: 6.0,
            formant_semitones,
            quality,
            filters: Vec::new(),
        }
    }

    #[test]
    fn builtin_squirrel_tracks_pitch_with_formants() {
        let squirrel = builtin_presets()
            .into_iter()
            .find(|preset| preset.id == PRESET_ARDILLA)
            .unwrap();
        assert_eq!(squirrel.pitch_semitones, 6.0);
        assert_eq!(squirrel.formant_semitones, 6.0);
        assert_eq!(squirrel.quality, Quality::Balanced);
    }

    #[test]
    fn migration_preserves_effective_quality_and_fixes_squirrel() {
        let mut store = PresetStore {
            version: 1,
            presets: vec![
                preset(PRESET_ARDILLA, true, Quality::High, 3.0),
                preset(PRESET_GRAVE, true, Quality::Ultra, -2.0),
                preset("custom_high", false, Quality::High, 0.0),
                preset("custom_balanced", false, Quality::Balanced, 0.0),
            ],
        };
        store.migrate(1);
        let squirrel = store.find(PRESET_ARDILLA).unwrap();
        assert_eq!(squirrel.formant_semitones, 6.0);
        assert_eq!(squirrel.quality, Quality::Balanced);
        assert_eq!(store.find(PRESET_GRAVE).unwrap().quality, Quality::Balanced);
        // Old High meant 2048 samples, which is the new Balanced setting.
        assert_eq!(
            store.find("custom_high").unwrap().quality,
            Quality::Balanced
        );
        // Old Balanced meant 1024 samples, which is the new Ultra setting.
        assert_eq!(
            store.find("custom_balanced").unwrap().quality,
            Quality::Ultra
        );
    }

    #[test]
    fn builtin_presets_count_and_unique_ids() {
        let builtins = builtin_presets();
        assert!(builtins.len() >= 14, "should have at least 14 studio presets");
        let mut ids = std::collections::HashSet::new();
        for p in &builtins {
            assert!(!p.id.is_empty());
            assert!(!p.name.is_empty());
            assert!(p.builtin);
            assert!(ids.insert(&p.id), "duplicate preset ID found: {}", p.id);
        }
    }

    #[test]
    fn all_filter_kinds_supported() {
        let kinds = [
            "noise_reduction",
            "gain",
            "speaker_lock",
            "noise_gate",
            "high_pass",
            "equalizer",
            "compressor",
            "chorus",
            "robotizer",
            "ring_mod",
            "distortion",
            "reverb",
            "humanizer",
        ];
        let mut p = preset("test", false, Quality::Balanced, 0.0);
        for kind in kinds {
            p.add_filter(kind);
            assert!(p.has_filter(kind), "filter {kind} should be added");
        }
        assert_eq!(p.filters.len(), kinds.len());
    }

    #[test]
    fn backup_unreadable_file_creates_backup() {
        let temp_dir = std::env::temp_dir().join(format!("mvx_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let corrupt_file = temp_dir.join("corrupt.toml");
        std::fs::write(&corrupt_file, "this is [not valid toml !!!").unwrap();
        crate::backup_unreadable_file(&corrupt_file);

        let mut found_bak = false;
        if let Ok(entries) = std::fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("corrupt.toml.") && name.ends_with(".bak") {
                    found_bak = true;
                    break;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&temp_dir);
        assert!(found_bak, "corrupted file backup should have been created");
    }
}
