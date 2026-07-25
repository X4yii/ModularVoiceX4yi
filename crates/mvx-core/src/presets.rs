//! Voice presets and their optional filter chain.

use serde::{Deserialize, Serialize};

use crate::{presets_path, Quality};

pub const PRESETS_VERSION: u32 = 2;
pub const DEFAULT_NOISE_STRENGTH: f32 = 0.75;

pub const PRESET_NATURAL: &str = "natural";
pub const PRESET_GRAVE: &str = "voz_grave";
pub const PRESET_ARDILLA: &str = "voz_ardilla";

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
}

impl FilterConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoiseReduction { .. } => "noise_reduction",
            Self::Gain { .. } => "gain",
            Self::SpeakerLock { .. } => "speaker_lock",
        }
    }

    pub fn enabled(&self) -> bool {
        match self {
            Self::NoiseReduction { enabled, .. } => *enabled,
            Self::Gain { enabled, .. } => *enabled,
            Self::SpeakerLock { enabled, .. } => *enabled,
        }
    }

    pub fn set_enabled(&mut self, value: bool) {
        match self {
            Self::NoiseReduction { enabled, .. } => *enabled = value,
            Self::Gain { enabled, .. } => *enabled = value,
            Self::SpeakerLock { enabled, .. } => *enabled = value,
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
            _ => return,
        };
        self.filters.push(filter);
    }
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
            name: "Natural".into(),
            builtin: true,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            filters: vec![
                FilterConfig::NoiseReduction {
                    enabled: false,
                    strength: default_strength(),
                },
                FilterConfig::Gain {
                    enabled: true,
                    gain_db: 0.0,
                },
                FilterConfig::SpeakerLock {
                    enabled: false,
                    profile: String::new(),
                    threshold: default_threshold(),
                    hold_ms: default_hold_ms(),
                    lookahead_ms: default_lookahead_ms(),
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
                FilterConfig::NoiseReduction {
                    enabled: false,
                    strength: default_strength(),
                },
                FilterConfig::Gain {
                    enabled: true,
                    gain_db: 0.0,
                },
                FilterConfig::SpeakerLock {
                    enabled: false,
                    profile: String::new(),
                    threshold: default_threshold(),
                    hold_ms: default_hold_ms(),
                    lookahead_ms: default_lookahead_ms(),
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
                FilterConfig::NoiseReduction {
                    enabled: false,
                    strength: default_strength(),
                },
                FilterConfig::Gain {
                    enabled: true,
                    gain_db: -3.0,
                },
                FilterConfig::SpeakerLock {
                    enabled: false,
                    profile: String::new(),
                    threshold: default_threshold(),
                    hold_ms: default_hold_ms(),
                    lookahead_ms: default_lookahead_ms(),
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
}
