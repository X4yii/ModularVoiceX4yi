use std::path::{Path, PathBuf};
use std::process::Command;
use serde::{Deserialize, Serialize};
use anyhow::{bail, Context, Result};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneStatus {
    Idle,
    Validating,
    Extracting,
    Ready,
    Synthesizing,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoiceCloneSession {
    pub name: String,
    pub source_path: PathBuf,
    pub sample_rate: u32,
    pub duration_secs: f32,
    pub status: CloneStatus,
    pub status_message: String,
    pub pitch_offset: f32,
    pub formant_offset: f32,
    pub similarity_blend: f32, // 0.0 to 1.0
    pub denoise_strength: f32, // 0.0 to 1.0
    pub model_target: String,  // e.g., "ONNX Neural Timbre"
    #[serde(default)]
    pub eq_low: f32,
    #[serde(default)]
    pub eq_mid: f32,
    #[serde(default)]
    pub eq_high: f32,
}

impl Default for VoiceCloneSession {
    fn default() -> Self {
        Self {
            name: "Voz_Clonada_1".into(),
            source_path: PathBuf::new(),
            sample_rate: 48_000,
            duration_secs: 0.0,
            status: CloneStatus::Idle,
            status_message: "Seleccioná un archivo WAV/MP3/OGG o grabá una muestra".into(),
            pitch_offset: 0.0,
            formant_offset: 0.0,
            similarity_blend: 0.85,
            denoise_strength: 0.75,
            model_target: "ONNX Neural Timbre / CAM++".into(),
            eq_low: 0.0,
            eq_mid: 0.0,
            eq_high: 0.0,
        }
    }
}

/// Expands tildes (`~`) and performs case-insensitive directory lookups
/// (e.g., `~/descargas/...` -> `~/Descargas/...`).
pub fn resolve_path(input: &Path) -> PathBuf {
    let raw = input.to_string_lossy();
    let expanded = if raw.starts_with('~') {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = raw.trim_start_matches('~').trim_start_matches('/');
            PathBuf::from(home).join(rest)
        } else {
            input.to_path_buf()
        }
    } else {
        input.to_path_buf()
    };

    if expanded.exists() {
        return expanded;
    }

    // Try case-insensitive matching for the parent directory (e.g. descargas -> Descargas)
    if let (Some(parent), Some(filename)) = (expanded.parent(), expanded.file_name()) {
        if let Some(grandparent) = parent.parent() {
            if let Ok(entries) = std::fs::read_dir(grandparent) {
                let parent_lower = parent
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase();
                for entry in entries.flatten() {
                    let entry_lower = entry.file_name().to_string_lossy().to_lowercase();
                    if entry_lower == parent_lower {
                        let candidate = entry.path().join(filename);
                        if candidate.exists() {
                            return candidate;
                        }
                    }
                }
            }
        }
    }

    expanded
}

/// Decodes audio using ffmpeg and extracts duration, pitch F0, suggested pitch/formants, and spectral balance.
pub fn analyze_audio_sample(path: &Path) -> Result<(f32, u32, f32, f32, f32, f32, f32, f32)> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-ar", "16000", "-ac", "1", "-f", "s16le", "-"])
        .output()
        .context("no se pudo ejecutar ffmpeg para analizar la muestra de voz")?;

    if !output.status.success() {
        bail!("ffmpeg no pudo decodificar el archivo de audio");
    }

    let bytes = output.stdout;
    if bytes.len() < 3200 {
        bail!("El archivo de audio es demasiado corto o está vacío");
    }

    let sample_rate = 16_000u32;
    let sample_count = bytes.len() / 2;
    let duration = sample_count as f32 / sample_rate as f32;

    let mut samples = Vec::with_capacity(sample_count);
    for chunk in bytes.chunks_exact(2) {
        let val = i16::from_le_bytes([chunk[0], chunk[1]]);
        samples.push(val as f32 / 32768.0);
    }

    // Pitch detection via autocorrelation on voiced segments
    let frame_len = 512;
    let hop = 256;
    let min_lag = (sample_rate / 400) as usize; // up to 400 Hz
    let max_lag = (sample_rate / 65) as usize;  // down to 65 Hz
    let mut pitches = Vec::new();

    let mut low_energy = 0.0f32;
    let mut mid_energy = 0.0f32;
    let mut high_energy = 0.0f32;

    let mut i = 0;
    while i + frame_len <= samples.len() {
        let chunk = &samples[i..i + frame_len];
        let mut sum_sq = 0.0f32;
        for &s in chunk {
            sum_sq += s * s;
        }
        let rms = (sum_sq / frame_len as f32).sqrt();
        if rms >= 0.030 {
            // Autocorrelation
            let mut best_corr = -1.0f32;
            let mut best_lag = 0;
            for lag in min_lag..max_lag.min(frame_len / 2) {
                let mut corr = 0.0f32;
                for j in 0..(frame_len - lag) {
                    corr += chunk[j] * chunk[j + lag];
                }
                let norm_corr = corr / (sum_sq + 1e-6);
                if norm_corr > best_corr {
                    best_corr = norm_corr;
                    best_lag = lag;
                }
            }
            if best_corr > 0.40 && best_lag > 0 {
                let f0 = sample_rate as f32 / best_lag as f32;
                pitches.push(f0);
            }

            // Spectral balance approximation
            let mut zc = 0;
            for j in 0..(frame_len - 1) {
                if (chunk[j] >= 0.0 && chunk[j + 1] < 0.0) || (chunk[j] < 0.0 && chunk[j + 1] >= 0.0) {
                    zc += 1;
                }
            }
            let zcr = zc as f32 / frame_len as f32;
            if zcr < 0.07 {
                low_energy += rms;
            } else if zcr < 0.22 {
                mid_energy += rms;
            } else {
                high_energy += rms;
            }
        }
        i += hop;
    }

    let target_f0 = if !pitches.is_empty() {
        pitches.sort_by(|a, b| a.partial_cmp(b).unwrap());
        pitches[pitches.len() / 2]
    } else {
        130.0
    };

    // Calculate suggested pitch offset relative to standard baseline (~175 Hz)
    let ref_f0 = 175.0f32;
    let pitch_offset = (12.0 * (target_f0 / ref_f0).log2()).clamp(-12.0, 12.0);
    let formant_offset = (pitch_offset * 0.65).clamp(-12.0, 12.0);

    let tot_e = low_energy + mid_energy + high_energy + 1e-6;
    let eq_low = (((low_energy / tot_e) - 0.33) * 10.0).clamp(-5.0, 5.0);
    let eq_mid = (((mid_energy / tot_e) - 0.33) * 10.0).clamp(-5.0, 5.0);
    let eq_high = (((high_energy / tot_e) - 0.33) * 10.0).clamp(-5.0, 5.0);

    Ok((
        duration,
        sample_rate,
        target_f0,
        pitch_offset,
        formant_offset,
        eq_low,
        eq_mid,
        eq_high,
    ))
}

impl VoiceCloneSession {
    pub fn validate_source(&mut self, path: &Path) -> Result<()> {
        let resolved = resolve_path(path);
        if !resolved.exists() {
            bail!("El archivo de audio no existe: {}", path.display());
        }
        let ext = resolved
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !matches!(ext.as_str(), "wav" | "mp3" | "ogg" | "flac") {
            bail!("Formato no soportado. Usá WAV, MP3, OGG o FLAC.");
        }

        self.source_path = resolved.clone();

        match analyze_audio_sample(&resolved) {
            Ok((dur, sr, f0, pitch, formant, eq_l, eq_m, eq_h)) => {
                self.duration_secs = dur;
                self.sample_rate = sr;
                self.pitch_offset = (pitch * 10.0).round() / 10.0;
                self.formant_offset = (formant * 10.0).round() / 10.0;
                self.eq_low = (eq_l * 10.0).round() / 10.0;
                self.eq_mid = (eq_m * 10.0).round() / 10.0;
                self.eq_high = (eq_h * 10.0).round() / 10.0;
                self.status = CloneStatus::Ready;
                self.status_message = format!(
                    "Muestra: {:.1}s | F0: {:.0}Hz | Pitch: {:+.1}st | Formant: {:+.1}st",
                    dur, f0, self.pitch_offset, self.formant_offset
                );
            }
            Err(_) => {
                self.duration_secs = 0.0;
                self.status = CloneStatus::Ready;
                self.status_message = format!(
                    "Muestra lista: {}",
                    resolved.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }

        Ok(())
    }

    pub fn to_preset(&self) -> crate::Preset {
        use crate::presets::FilterConfig;

        crate::Preset {
            id: format!("clone_{}", self.name.to_lowercase().replace(' ', "_")),
            name: format!("IA: {}", self.name),
            builtin: false,
            pitch_semitones: self.pitch_offset,
            formant_semitones: self.formant_offset,
            quality: crate::Quality::Balanced,
            filters: vec![
                FilterConfig::HighPass {
                    enabled: true,
                    cutoff_hz: 80.0,
                },
                FilterConfig::NoiseReduction {
                    enabled: true,
                    strength: self.denoise_strength,
                },
                FilterConfig::Equalizer {
                    enabled: true,
                    low_db: self.eq_low,
                    mid_db: self.eq_mid,
                    high_db: self.eq_high,
                },
                FilterConfig::Compressor {
                    enabled: true,
                    threshold_db: -18.0,
                    ratio: 3.5,
                    attack_ms: 15.0,
                    release_ms: 120.0,
                    makeup_db: 2.5,
                },
                FilterConfig::Gain {
                    enabled: true,
                    gain_db: 0.0,
                },
            ],
        }
    }
}
