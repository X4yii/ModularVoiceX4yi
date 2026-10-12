use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::{bail, Result};

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
}

impl Default for VoiceCloneSession {
    fn default() -> Self {
        Self {
            name: "Voz_Clonada_1".into(),
            source_path: PathBuf::new(),
            sample_rate: 48_000,
            duration_secs: 0.0,
            status: CloneStatus::Idle,
            status_message: "Seleccioná un archivo WAV/MP3 o grabá una muestra".into(),
            pitch_offset: 0.0,
            formant_offset: 0.0,
            similarity_blend: 0.85,
            denoise_strength: 0.50,
            model_target: "ONNX Neural Timbre / CAM++".into(),
        }
    }
}

impl VoiceCloneSession {
    pub fn validate_source(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            bail!("El archivo de audio no existe: {}", path.display());
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !matches!(ext.as_str(), "wav" | "mp3" | "ogg" | "flac") {
            bail!("Formato no soportado. Usá WAV, MP3, OGG o FLAC.");
        }
        self.source_path = path.to_path_buf();
        self.status = CloneStatus::Ready;
        self.status_message = format!(
            "Muestra lista: {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        Ok(())
    }

    pub fn to_preset(&self) -> crate::Preset {
        let mut p = crate::Preset {
            id: format!("clone_{}", self.name.to_lowercase().replace(' ', "_")),
            name: format!("IA: {}", self.name),
            builtin: false,
            pitch_semitones: self.pitch_offset,
            formant_semitones: self.formant_offset,
            quality: crate::Quality::Balanced,
            filters: Vec::new(),
        };
        p.add_filter("noise_reduction");
        p.add_filter("compressor");
        p
    }
}
