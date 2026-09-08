//! Soundboard clip loading, WAV decoding, and synthesis for built-in effects.

use std::fs;

use crate::{sounds_dir, SoundClipInfo};

const TARGET_SAMPLE_RATE: u32 = 48_000;

#[derive(Clone, Debug)]
pub struct LoadedSound {
    pub id: String,
    pub name: String,
    pub samples: Vec<f32>, // 48 kHz mono
    pub duration_secs: f32,
    pub builtin: bool,
}

impl LoadedSound {
    pub fn to_info(&self) -> SoundClipInfo {
        SoundClipInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            duration_secs: self.duration_secs,
            builtin: self.builtin,
        }
    }
}

/// Decode any standard PCM (8/16/24/32 bit) or IEEE float WAV file and convert to 48kHz mono f32.
pub fn decode_wav(bytes: &[u8]) -> anyhow::Result<Vec<f32>> {
    if bytes.len() < 44 {
        anyhow::bail!("archivo WAV demasiado corto");
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("cabecera RIFF/WAVE no válida");
    }

    let mut pos = 12;
    let mut audio_format = 1u16; // 1 = PCM, 3 = IEEE float
    let mut channels = 1u16;
    let mut sample_rate = TARGET_SAMPLE_RATE;
    let mut bits_per_sample = 16u16;
    let mut data_offset = 0usize;
    let mut data_size = 0usize;

    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let chunk_data = pos + 8;
        if chunk_data + chunk_size > bytes.len() {
            break;
        }

        if chunk_id == b"fmt " && chunk_size >= 16 {
            audio_format = u16::from_le_bytes(bytes[chunk_data..chunk_data + 2].try_into()?);
            channels = u16::from_le_bytes(bytes[chunk_data + 2..chunk_data + 4].try_into()?);
            sample_rate = u32::from_le_bytes(bytes[chunk_data + 4..chunk_data + 8].try_into()?);
            bits_per_sample =
                u16::from_le_bytes(bytes[chunk_data + 14..chunk_data + 16].try_into()?);
        } else if chunk_id == b"data" {
            data_offset = chunk_data;
            data_size = chunk_size;
            break;
        }
        pos = chunk_data + chunk_size;
    }

    if data_offset == 0 || data_size == 0 {
        anyhow::bail!("no se encontró el chunk data en el WAV");
    }

    let raw_data = &bytes[data_offset..data_offset + data_size];
    let ch = channels.max(1) as usize;
    let mut raw_mono = Vec::new();

    match (audio_format, bits_per_sample) {
        (1, 16) => {
            let bytes_per_frame = ch * 2;
            let num_frames = raw_data.len() / bytes_per_frame;
            raw_mono.reserve(num_frames);
            for f in 0..num_frames {
                let mut sum = 0.0f32;
                for c in 0..ch {
                    let idx = f * bytes_per_frame + c * 2;
                    let s = i16::from_le_bytes(raw_data[idx..idx + 2].try_into()?);
                    sum += s as f32 / 32768.0;
                }
                raw_mono.push(sum / ch as f32);
            }
        }
        (1, 8) => {
            let bytes_per_frame = ch;
            let num_frames = raw_data.len() / bytes_per_frame;
            raw_mono.reserve(num_frames);
            for f in 0..num_frames {
                let mut sum = 0.0f32;
                for c in 0..ch {
                    let idx = f * bytes_per_frame + c;
                    let s = raw_data[idx] as f32 - 128.0;
                    sum += s / 128.0;
                }
                raw_mono.push(sum / ch as f32);
            }
        }
        (1, 24) => {
            let bytes_per_frame = ch * 3;
            let num_frames = raw_data.len() / bytes_per_frame;
            raw_mono.reserve(num_frames);
            for f in 0..num_frames {
                let mut sum = 0.0f32;
                for c in 0..ch {
                    let idx = f * bytes_per_frame + c * 3;
                    let b = [0, raw_data[idx], raw_data[idx + 1], raw_data[idx + 2]];
                    let s = i32::from_le_bytes(b) >> 8;
                    sum += s as f32 / 8388608.0;
                }
                raw_mono.push(sum / ch as f32);
            }
        }
        (3, 32) | (1, 32) => {
            let bytes_per_frame = ch * 4;
            let num_frames = raw_data.len() / bytes_per_frame;
            raw_mono.reserve(num_frames);
            for f in 0..num_frames {
                let mut sum = 0.0f32;
                for c in 0..ch {
                    let idx = f * bytes_per_frame + c * 4;
                    let s = f32::from_le_bytes(raw_data[idx..idx + 4].try_into()?);
                    sum += s;
                }
                raw_mono.push(sum / ch as f32);
            }
        }
        _ => anyhow::bail!(
            "formato WAV no soportado: formato={audio_format}, bits={bits_per_sample}"
        ),
    }

    // Resample to TARGET_SAMPLE_RATE (48kHz) via linear interpolation if needed
    if sample_rate == TARGET_SAMPLE_RATE || raw_mono.is_empty() {
        Ok(raw_mono)
    } else {
        let ratio = sample_rate as f64 / TARGET_SAMPLE_RATE as f64;
        let target_len = (raw_mono.len() as f64 / ratio).round() as usize;
        let mut resampled = Vec::with_capacity(target_len);
        for i in 0..target_len {
            let src_idx = i as f64 * ratio;
            let i0 = src_idx.floor() as usize;
            let i1 = (i0 + 1).min(raw_mono.len() - 1);
            let frac = (src_idx - i0 as f64) as f32;
            let s = raw_mono[i0] + frac * (raw_mono[i1] - raw_mono[i0]);
            resampled.push(s);
        }
        Ok(resampled)
    }
}

/// Encode 48kHz mono float audio into a standard 16-bit PCM WAV.
pub fn encode_wav_i16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let num_samples = samples.len() as u32;
    let data_size = num_samples * 2;
    let file_size = 36 + data_size;

    let mut buf = Vec::with_capacity(44 + data_size as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // subchunk1size (16 for PCM)
    buf.extend_from_slice(&1u16.to_le_bytes()); // audio format 1 (PCM)
    buf.extend_from_slice(&1u16.to_le_bytes()); // num channels 1 (mono)
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * 2;
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());

    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let val = (clamped * 32767.0) as i16;
        buf.extend_from_slice(&val.to_le_bytes());
    }
    buf
}

/// Synthesize built-in high quality soundboard effects.
pub fn generate_builtin_sounds() -> Vec<(&'static str, &'static str, Vec<f32>)> {
    let sr = TARGET_SAMPLE_RATE as f32;

    // 1. Airhorn (Bocina estadio, acorde potente ~0.7s)
    let airhorn_len = (sr * 0.7) as usize;
    let mut airhorn = Vec::with_capacity(airhorn_len);
    let freqs = [466.16, 587.33, 698.46, 932.33]; // Bb chord
    for i in 0..airhorn_len {
        let t = i as f32 / sr;
        let mut s = 0.0f32;
        for (idx, &f) in freqs.iter().enumerate() {
            let weight = 1.0 / (idx + 1) as f32;
            s += (2.0 * std::f32::consts::PI * f * t).sin() * weight;
            // Add harmonic overtone
            s += (4.0 * std::f32::consts::PI * f * t).sin() * (weight * 0.4);
        }
        let env = if t < 0.03 {
            t / 0.03
        } else if t > 0.55 {
            (0.7 - t).max(0.0) / 0.15
        } else {
            1.0
        };
        airhorn.push((s * 0.28 * env).clamp(-1.0, 1.0));
    }

    // 2. Bleep (Censura 1000 Hz ~0.4s)
    let bleep_len = (sr * 0.4) as usize;
    let mut bleep = Vec::with_capacity(bleep_len);
    for i in 0..bleep_len {
        let t = i as f32 / sr;
        let s = (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
        let env = if t < 0.01 {
            t / 0.01
        } else if t > 0.38 {
            (0.4 - t).max(0.0) / 0.02
        } else {
            1.0
        };
        bleep.push(s * 0.35 * env);
    }

    // 3. Chime (Campana positiva brillante)
    let chime_len = (sr * 0.8) as usize;
    let mut chime = Vec::with_capacity(chime_len);
    for i in 0..chime_len {
        let t = i as f32 / sr;
        let f = if t < 0.15 { 880.0 } else { 1760.0 };
        let env = (-t * 3.5).exp();
        let s = (2.0 * std::f32::consts::PI * f * t).sin() * 0.35 * env;
        chime.push(s);
    }

    // 4. Buzzer (Error buzzer grave)
    let buzzer_len = (sr * 0.45) as usize;
    let mut buzzer = Vec::with_capacity(buzzer_len);
    for i in 0..buzzer_len {
        let t = i as f32 / sr;
        let s = if (t * 110.0).fract() > 0.5 { 0.3 } else { -0.3 };
        let env = if t > 0.4 {
            (0.45 - t).max(0.0) / 0.05
        } else {
            1.0
        };
        buzzer.push(s * env);
    }

    vec![
        ("airhorn", "Airhorn (Bocina)", airhorn),
        ("bleep", "Bleep Censura (1 kHz)", bleep),
        ("chime", "Campana Notificación", chime),
        ("buzzer", "Buzzer Error", buzzer),
    ]
}

/// Ensures the sounds directory exists and populates it with the default sound effects if empty.
pub fn ensure_builtin_sounds() -> anyhow::Result<()> {
    let dir = sounds_dir();
    fs::create_dir_all(&dir)?;

    let builtins = generate_builtin_sounds();
    for (id, _name, samples) in builtins {
        let file_path = dir.join(format!("{id}.wav"));
        if !file_path.exists() {
            let wav_bytes = encode_wav_i16(&samples, TARGET_SAMPLE_RATE);
            let _ = fs::write(file_path, wav_bytes);
        }
    }
    Ok(())
}

/// Scan `sounds_dir()` and load all `.wav` sound files.
pub fn load_all_sounds() -> Vec<LoadedSound> {
    let _ = ensure_builtin_sounds();
    let mut list = Vec::new();
    let dir = sounds_dir();

    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("wav") {
                if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(bytes) = fs::read(&path) {
                        if let Ok(samples) = decode_wav(&bytes) {
                            let duration = samples.len() as f32 / TARGET_SAMPLE_RATE as f32;
                            let is_builtin = matches!(id, "airhorn" | "bleep" | "chime" | "buzzer");
                            let name = match id {
                                "airhorn" => "Airhorn (Bocina)",
                                "bleep" => "Bleep Censura",
                                "chime" => "Campana / Chime",
                                "buzzer" => "Buzzer Error",
                                custom => custom,
                            }
                            .to_string();

                            list.push(LoadedSound {
                                id: id.to_string(),
                                name,
                                samples,
                                duration_secs: duration,
                                builtin: is_builtin,
                            });
                        }
                    }
                }
            }
        }
    }

    list.sort_by(|a, b| {
        if a.builtin != b.builtin {
            b.builtin.cmp(&a.builtin)
        } else {
            a.name.cmp(&b.name)
        }
    });

    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_encode_and_decode_roundtrip() {
        let original: Vec<f32> = (0..4800)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin() * 0.5)
            .collect();
        let encoded = encode_wav_i16(&original, 48000);
        assert!(encoded.len() > 44);

        let decoded = decode_wav(&encoded).unwrap();
        assert_eq!(decoded.len(), original.len());
        for (a, b) in original.iter().zip(decoded.iter()) {
            assert!((a - b).abs() < 0.001, "PCM roundtrip precision error");
        }
    }

    #[test]
    fn builtin_sounds_generation() {
        let sounds = generate_builtin_sounds();
        assert_eq!(sounds.len(), 4);
        for (id, name, samples) in sounds {
            assert!(!id.is_empty());
            assert!(!name.is_empty());
            assert!(!samples.is_empty());
            assert!(samples.iter().all(|s| s.abs() <= 1.0));
        }
    }
}
