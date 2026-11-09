//! Neural Voice Conversion using OpenVoice V2 ONNX models and tract.

use anyhow::{bail, Context, Result};
use ndarray::Array2;
use realfft::{RealFftPlanner, RealToComplex};
use rtrb::{Consumer, Producer, RingBuffer};
use sherpa_onnx::LinearResampler;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tract_onnx::prelude::*;

const TARGET_RATE: i32 = 22_050;
const N_FFT: usize = 1024;
const HOP_LENGTH: usize = 256;
const PADDING: usize = 384;
const N_BINS: usize = N_FFT / 2 + 1; // 513

type TractRunnable = Arc<TypedRunnableModel>;

pub struct NeuralVoiceConverter {
    extract_model: TractRunnable,
    color_model: TractRunnable,
    downsampler: LinearResampler,
    upsampler: LinearResampler,
    fft: Arc<dyn RealToComplex<f32>>,
    hann_window: Vec<f32>,
}

impl NeuralVoiceConverter {
    pub fn new(models_dir: &Path) -> Result<Self> {
        let extract_path = models_dir.join("openvoice/tone_extract.onnx");
        let color_path = models_dir.join("openvoice/tone_color.onnx");

        if !extract_path.exists() {
            bail!(
                "No se encontró el modelo tone_extract en {}",
                extract_path.display()
            );
        }
        if !color_path.exists() {
            bail!(
                "No se encontró el modelo tone_color en {}",
                color_path.display()
            );
        }

        // Load models directly; dynamic input shapes are preserved from the ONNX graph
        let extract_model = tract_onnx::onnx()
            .model_for_path(&extract_path)
            .context("cargando tone_extract.onnx")?
            .into_optimized()?
            .into_runnable()?;

        let color_model = tract_onnx::onnx()
            .model_for_path(&color_path)
            .context("cargando tone_color.onnx")?
            .into_optimized()?
            .into_runnable()?;

        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(N_FFT);

        // Periodic Hann window of length 1024
        let mut hann_window = Vec::with_capacity(N_FFT);
        for i in 0..N_FFT {
            let val = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / N_FFT as f32).cos());
            hann_window.push(val);
        }

        let downsampler =
            LinearResampler::create(48_000, TARGET_RATE).context("creando resampler 48k -> 22k")?;
        let upsampler =
            LinearResampler::create(TARGET_RATE, 48_000).context("creando resampler 22k -> 48k")?;

        Ok(Self {
            extract_model,
            color_model,
            downsampler,
            upsampler,
            fft,
            hann_window,
        })
    }

    /// Computes STFT linear magnitude spectrogram matching OpenVoice specification.
    /// Returns 2D array [frames, 513].
    pub fn compute_spectrogram(&self, samples_22k: &[f32]) -> Result<Array2<f32>> {
        if samples_22k.len() < N_FFT {
            bail!("Audio demasiado corto para STFT (mínimo 1024 muestras)");
        }

        // Reflect padding of 384 samples on both sides
        let mut padded = Vec::with_capacity(samples_22k.len() + 2 * PADDING);
        for i in (1..=PADDING).rev() {
            let idx = if i < samples_22k.len() { i } else { 0 };
            padded.push(samples_22k[idx]);
        }
        padded.extend_from_slice(samples_22k);
        for i in 1..=PADDING {
            let idx = samples_22k.len().saturating_sub(1 + i);
            padded.push(samples_22k[idx]);
        }

        let total_samples = padded.len();
        if total_samples < N_FFT {
            bail!("Audio con padding demasiado corto");
        }

        let num_frames = (total_samples - N_FFT) / HOP_LENGTH + 1;
        let mut spec = Array2::<f32>::zeros((num_frames, N_BINS));

        let mut in_buf = vec![0.0f32; N_FFT];
        let mut out_spectrum = self.fft.make_output_vec();

        for frame_idx in 0..num_frames {
            let start = frame_idx * HOP_LENGTH;
            for j in 0..N_FFT {
                in_buf[j] = padded[start + j] * self.hann_window[j];
            }

            self.fft
                .process(&mut in_buf, &mut out_spectrum)
                .map_err(|e| anyhow::anyhow!("FFT falló: {e:?}"))?;

            for bin in 0..N_BINS {
                let re = out_spectrum[bin].re;
                let im = out_spectrum[bin].im;
                spec[[frame_idx, bin]] = (re * re + im * im + 1e-6).sqrt();
            }
        }

        Ok(spec)
    }

    /// Extracts 256-dimensional tone color embedding from a 22050 Hz audio sample.
    pub fn extract_embedding_22k(&self, samples_22k: &[f32]) -> Result<Vec<f32>> {
        let spec = self.compute_spectrogram(samples_22k)?;
        let num_frames = spec.nrows();

        // Input shape: [1, frames, 513]
        let mut raw = Vec::with_capacity(num_frames * N_BINS);
        for f in 0..num_frames {
            for b in 0..N_BINS {
                raw.push(spec[[f, b]]);
            }
        }

        let tract_tensor =
            tract_ndarray::Array3::from_shape_vec((1, num_frames, N_BINS), raw)?.into_tensor();

        let results = self.extract_model.run(tvec!(tract_tensor.into()))?;
        let emb: Vec<f32> = results[0]
            .to_plain_array_view::<f32>()?
            .iter()
            .copied()
            .take(256)
            .collect();

        Ok(emb)
    }

    /// Extracts embedding from 48000 Hz audio.
    pub fn extract_embedding_48k(&self, samples_48k: &[f32]) -> Result<Vec<f32>> {
        let samples_22k = self.downsampler.resample(samples_48k, true);
        self.extract_embedding_22k(&samples_22k)
    }

    /// Converts source speech to target tone color using OpenVoice V2.
    /// Input: source samples at 48000 Hz. Output: converted audio at 48000 Hz.
    pub fn convert_voice_48k(
        &self,
        source_48k: &[f32],
        src_tone: &[f32],
        dest_tone: &[f32],
        tau: f32,
    ) -> Result<Vec<f32>> {
        if source_48k.len() < 2400 {
            return Ok(source_48k.to_vec());
        }

        let src_22k = self.downsampler.resample(source_48k, true);
        let spec = self.compute_spectrogram(&src_22k)?;
        let num_frames = spec.nrows();

        // Input 1: audio -> [1, 513, frames]
        let mut audio_raw = Vec::with_capacity(N_BINS * num_frames);
        for b in 0..N_BINS {
            for f in 0..num_frames {
                audio_raw.push(spec[[f, b]]);
            }
        }

        let t_audio = tract_ndarray::Array3::from_shape_vec((1, N_BINS, num_frames), audio_raw)?
            .into_tensor();

        // Input 2: audio_length -> [1] (i64)
        let t_len =
            tract_ndarray::Array1::from_shape_vec(1, vec![num_frames as i64])?.into_tensor();

        // Input 3: src_tone -> [1, 256, 1] (f32)
        let t_src =
            tract_ndarray::Array3::from_shape_vec((1, 256, 1), src_tone.to_vec())?.into_tensor();

        // Input 4: dest_tone -> [1, 256, 1] (f32)
        let t_dest =
            tract_ndarray::Array3::from_shape_vec((1, 256, 1), dest_tone.to_vec())?.into_tensor();

        // Input 5: tau -> [1] (f32)
        let t_tau = tract_ndarray::Array1::from_shape_vec(1, vec![tau])?.into_tensor();

        let results = self.color_model.run(tvec!(
            t_audio.into(),
            t_len.into(),
            t_src.into(),
            t_dest.into(),
            t_tau.into()
        ))?;

        // Output: [1, 1, length]
        let output_audio: Vec<f32> = results[0]
            .to_plain_array_view::<f32>()?
            .iter()
            .copied()
            .collect();

        let upsampled = self.upsampler.resample(&output_audio, true);
        Ok(upsampled)
    }
}

/// Decodes any audio file (OGG, MP3, WAV, FLAC) to mono f32 samples at the requested sample rate.
pub fn decode_audio_file(path: &Path, target_rate: u32) -> Result<Vec<f32>> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-ar",
            &target_rate.to_string(),
            "-ac",
            "1",
            "-f",
            "f32le",
            "-",
        ])
        .output()
        .context("ffmpeg no esta instalado o no se pudo ejecutar (requerido para leer la muestra de voz; instala ffmpeg)")?;

    if !output.status.success() {
        bail!("ffmpeg falló al decodificar: {}", path.display());
    }

    let bytes = output.stdout;
    let mut samples = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.as_chunks::<4>().0 {
        samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }

    Ok(samples)
}

/// Saves a 256-dim embedding vector to a binary or JSON file.
pub fn save_embedding(path: &Path, emb: &[f32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(emb)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Loads a 256-dim embedding vector from a JSON file.
pub fn load_embedding(path: &Path) -> Result<Vec<f32>> {
    let content = std::fs::read_to_string(path)?;
    let emb: Vec<f32> = serde_json::from_str(&content)?;
    if emb.len() != 256 {
        bail!(
            "Embedding inválido: esperado tamaño 256, recibido {}",
            emb.len()
        );
    }
    Ok(emb)
}

const VC_RING_CAPACITY: usize = 65_536;
const VC_CHUNK_SAMPLES: usize = 12_000; // 250 ms at 48 kHz

pub struct NeuralVoiceWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

pub struct NeuralVoiceStart {
    pub worker: Option<NeuralVoiceWorker>,
    pub producer: Option<Producer<f32>>,
    pub consumer: Option<Consumer<f32>>,
}

impl NeuralVoiceWorker {
    pub fn start(models_dir: &Path, target_emb: Vec<f32>, tau: f32) -> NeuralVoiceStart {
        if target_emb.len() != 256 {
            return NeuralVoiceStart {
                worker: None,
                producer: None,
                consumer: None,
            };
        }

        let converter = match NeuralVoiceConverter::new(models_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("fallo al instanciar NeuralVoiceConverter: {e}");
                return NeuralVoiceStart {
                    worker: None,
                    producer: None,
                    consumer: None,
                };
            }
        };

        let (in_prod, in_cons) = RingBuffer::new(VC_RING_CAPACITY);
        let (out_prod, out_cons) = RingBuffer::new(VC_RING_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        let handle = thread::Builder::new()
            .name("mvx-neural-vc".into())
            .spawn(move || {
                #[cfg(unix)]
                unsafe {
                    libc::nice(5);
                }
                run_neural_worker(converter, in_cons, out_prod, target_emb, tau, thread_stop);
            })
            .ok();

        NeuralVoiceStart {
            worker: Some(Self { stop, handle }),
            producer: Some(in_prod),
            consumer: Some(out_cons),
        }
    }
}

impl Drop for NeuralVoiceWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_neural_worker(
    converter: NeuralVoiceConverter,
    mut in_cons: Consumer<f32>,
    mut out_prod: Producer<f32>,
    target_emb: Vec<f32>,
    tau: f32,
    stop: Arc<AtomicBool>,
) {
    let mut in_buffer: Vec<f32> = Vec::with_capacity(VC_CHUNK_SAMPLES * 2);
    let mut source_emb: Option<Vec<f32>> = None;

    while !stop.load(Ordering::Relaxed) {
        while let Ok(sample) = in_cons.pop() {
            in_buffer.push(sample);
        }

        if in_buffer.len() >= VC_CHUNK_SAMPLES {
            let chunk = in_buffer.drain(..VC_CHUNK_SAMPLES).collect::<Vec<f32>>();

            let mut sum_sq = 0.0f32;
            for &s in &chunk {
                sum_sq += s * s;
            }
            let rms = (sum_sq / chunk.len() as f32).sqrt();

            if rms > 0.02 {
                if source_emb.is_none() {
                    if let Ok(emb) = converter.extract_embedding_48k(&chunk) {
                        source_emb = Some(emb);
                    }
                }

                let src = source_emb.as_ref().unwrap_or(&target_emb);
                match converter.convert_voice_48k(&chunk, src, &target_emb, tau) {
                    Ok(converted) => {
                        for s in converted {
                            let _ = out_prod.push(s);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("error en inferencia de conversion de voz: {e}");
                        for s in chunk {
                            let _ = out_prod.push(s);
                        }
                    }
                }
            } else {
                for s in chunk {
                    let _ = out_prod.push(s);
                }
            }
        } else {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_extract_and_save_voice_message() {
        // Test de integracion opcional: requiere modelos y MVX_TEST_VOICE=<archivo>.
        let models_dir = mvx_core::models_dir();
        let Some(ogg_path) = std::env::var_os("MVX_TEST_VOICE").map(PathBuf::from) else {
            eprintln!("Saltando test: define MVX_TEST_VOICE con una muestra de voz");
            return;
        };

        if !models_dir.join("openvoice/tone_extract.onnx").exists() || !ogg_path.exists() {
            eprintln!("Saltando test porque faltan modelos o muestra");
            return;
        }

        let converter =
            NeuralVoiceConverter::new(&models_dir).expect("cargando NeuralVoiceConverter");
        let audio_48k =
            decode_audio_file(&ogg_path, 48_000).expect("decodificando voice-message.ogg");
        assert!(!audio_48k.is_empty());

        let emb = converter
            .extract_embedding_48k(&audio_48k)
            .expect("extrayendo embedding");
        assert_eq!(emb.len(), 256);

        let voices_dir = std::env::temp_dir().join("mvx-test-voices");
        let _ = std::fs::create_dir_all(&voices_dir);
        save_embedding(&voices_dir.join("test_voice.json"), &emb).expect("guardando embedding");

        let loaded =
            load_embedding(&voices_dir.join("test_voice.json")).expect("cargando embedding");
        assert_eq!(loaded.len(), 256);

        // Test voice conversion with a short segment (e.g. 24000 samples = 0.5s)
        let test_src = &audio_48k[..24_000.min(audio_48k.len())];
        let converted = converter
            .convert_voice_48k(test_src, &emb, &emb, 0.0)
            .expect("convirtiendo voz");
        assert!(!converted.is_empty());
        println!(
            "Test completado con éxito. Muestras convertidas: {}",
            converted.len()
        );
    }
}
