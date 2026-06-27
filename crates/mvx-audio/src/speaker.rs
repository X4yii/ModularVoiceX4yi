//! Off-audio-thread speaker verification and enrollment.
//!
//! The realtime callback pushes raw mono samples into a small SPSC ring. This
//! worker resamples them to 16 kHz, runs Silero VAD and a CAM++ embedding model,
//! and publishes a single boolean decision for the DSP gate. It intentionally
//! runs at a low priority and with one ONNX thread.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};
use sherpa_onnx::{
    LinearResampler, SileroVadModelConfig, SpeakerEmbeddingExtractor,
    SpeakerEmbeddingExtractorConfig, VadModelConfig, VoiceActivityDetector,
};

use mvx_core::SharedState;

const INPUT_RATE: i32 = 48_000;
const ANALYSIS_RATE: i32 = 16_000;
const RING_CAPACITY: usize = 65_536;
const SAMPLE_CHUNK: usize = 4_800; // 100 ms at 48 kHz
const WINDOW_SAMPLES: usize = 8_000; // 0.5 s at 16 kHz

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoiceProfile {
    pub name: String,
    pub embedding: Vec<f32>,
    #[serde(default)]
    pub model: String,
}

pub struct SpeakerWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    enroll: Arc<Mutex<Option<EnrollRequest>>>,
}

#[derive(Clone, Debug)]
pub struct EnrollRequest {
    pub name: String,
    pub seconds: u32,
}

pub struct SpeakerStart {
    pub worker: Option<SpeakerWorker>,
    pub producer: Option<Producer<f32>>,
}

impl SpeakerWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        shared: Arc<SharedState>,
        profile_name: String,
        model: PathBuf,
        vad_model: PathBuf,
        analysis_hz: u32,
    ) -> SpeakerStart {
        if !model.exists() {
            shared.set_speaker_enabled(false, false);
            shared.set_speaker_status(format!(
                "falta el modelo de voz: {} (ejecutá «mxv models install»)",
                model.display()
            ));
            return SpeakerStart {
                worker: None,
                producer: None,
            };
        }
        // A missing or malformed profile is not fatal: the worker still needs
        // to run so the user can enroll a voice from the TUI.
        let profile = match load_profile(&profile_name) {
            Ok(profile) => profile,
            Err(err) => {
                shared.set_speaker_status(format!("perfil de voz inválido: {err}"));
                None
            }
        };

        let (producer, consumer) = RingBuffer::new(RING_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let enroll = Arc::new(Mutex::new(None));
        let thread_stop = stop.clone();
        let thread_enroll = enroll.clone();
        let thread_shared = shared.clone();
        let interval = Duration::from_millis(1000 / analysis_hz.max(1) as u64);
        let handle = thread::Builder::new()
            .name("mvx-speaker".into())
            .spawn(move || {
                // Best effort: lower the priority of the analysis thread when
                // the platform exposes nice(2) through libc.
                #[cfg(unix)]
                unsafe {
                    libc::nice(10);
                }
                if let Err(err) = run_worker(
                    thread_shared.clone(),
                    consumer,
                    profile,
                    model,
                    vad_model,
                    interval,
                    thread_stop,
                    thread_enroll,
                ) {
                    thread_shared.set_speaker_enabled(false, false);
                    thread_shared.set_speaker_status(format!("analizador de voz: {err:#}"));
                }
            })
            .ok();

        SpeakerStart {
            worker: Some(Self {
                stop,
                handle,
                enroll,
            }),
            producer: Some(producer),
        }
    }

    pub fn request_enrollment(&self, name: String, seconds: u32) {
        if let Ok(mut slot) = self.enroll.lock() {
            *slot = Some(EnrollRequest { name, seconds });
        }
    }
}

impl Drop for SpeakerWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    shared: Arc<SharedState>,
    mut consumer: Consumer<f32>,
    mut profile: Option<VoiceProfile>,
    model: PathBuf,
    vad_model: PathBuf,
    analysis_interval: Duration,
    stop: Arc<AtomicBool>,
    enroll_slot: Arc<Mutex<Option<EnrollRequest>>>,
) -> Result<()> {
    let extractor = SpeakerEmbeddingExtractor::create(&SpeakerEmbeddingExtractorConfig {
        model: Some(model.display().to_string()),
        num_threads: 1,
        debug: false,
        provider: Some("cpu".to_string()),
    })
    .context("no se pudo cargar el modelo de speaker embedding")?;
    let resampler = LinearResampler::create(INPUT_RATE, ANALYSIS_RATE)
        .context("no se pudo crear el resampler para el analizador de voz")?;
    let vad = if vad_model.exists() {
        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(vad_model.display().to_string()),
                threshold: 0.45,
                min_silence_duration: 0.20,
                min_speech_duration: 0.15,
                window_size: 512,
                max_speech_duration: 20.0,
            },
            sample_rate: ANALYSIS_RATE,
            num_threads: 1,
            provider: Some("cpu".to_string()),
            debug: false,
            ..Default::default()
        };
        VoiceActivityDetector::create(&config, 10.0)
    } else {
        None
    };

    let has_vad = vad_model.exists();
    shared.set_models_installed(model.exists() && has_vad);
    if profile.is_some() {
        shared.set_speaker_enabled(true, true);
        shared.set_speaker_metrics(1.0, true, true);
        shared.set_speaker_status("analizando voz");
    } else {
        shared.set_speaker_enabled(false, false);
        shared.set_speaker_metrics(0.0, false, false);
        shared.set_speaker_status("sin perfil de voz; usá «Enrolar voz» para crear uno");
    }

    let mut raw = Vec::with_capacity(SAMPLE_CHUNK);
    let mut analysis: VecDeque<f32> = VecDeque::with_capacity(16_000);
    let mut enrollment: Option<EnrollBuffer> = None;
    let mut last_analysis = Instant::now();
    let mut hold_until = Instant::now() + Duration::from_millis(shared.speaker_config().1 as u64);
    let mut last_vad = true;
    let mut last_score = 1.0f32;
    let mut last_drained = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Pick up enrollment requests. Enrollment is deliberately handled in
        // this worker so only one thread owns the ONNX session.
        if let Ok(mut slot) = enroll_slot.lock() {
            if let Some(request) = slot.take() {
                if request.name.trim().is_empty() {
                    shared.set_speaker_status("el nombre del perfil no puede estar vacío");
                } else {
                    let seconds = request.seconds.clamp(3, 30);
                    shared.set_speaker_status(format!(
                        "enrolando «{}»: esperá {seconds}s y hablá con naturalidad",
                        request.name
                    ));
                    enrollment = Some(EnrollBuffer {
                        name: request.name,
                        target: seconds as usize * ANALYSIS_RATE as usize,
                        samples: Vec::with_capacity(seconds as usize * ANALYSIS_RATE as usize),
                    });
                    shared.set_speaker_metrics(1.0, true, true);
                }
            }
        }

        // Drain the realtime ring. Each iteration of the outer loop handles a
        // bounded amount so a stalled analysis cannot spin forever.
        let mut drained = 0usize;
        while drained < SAMPLE_CHUNK * 8 {
            match consumer.pop() {
                Ok(sample) => {
                    raw.push(sample);
                    drained += 1;
                }
                Err(_) => break,
            }
        }
        if !raw.is_empty() {
            let resampled = resampler.resample(&raw, false);
            raw.clear();
            if !resampled.is_empty() {
                if let Some(vad) = &vad {
                    vad.accept_waveform(&resampled);
                    last_vad = vad.detected();
                } else {
                    let energy = resampled.iter().map(|v| v * v).sum::<f32>()
                        / resampled.len().max(1) as f32;
                    last_vad = energy > 0.0005;
                }

                for sample in &resampled {
                    analysis.push_back(*sample);
                }
                while analysis.len() > ANALYSIS_RATE as usize {
                    analysis.pop_front();
                }

                if let Some(enroll) = enrollment.as_mut() {
                    enroll.samples.extend_from_slice(&resampled);
                    if enroll.samples.len() >= enroll.target {
                        let samples = std::mem::take(&mut enroll.samples);
                        let embedding = compute_embedding(&extractor, &samples)?;
                        let new_profile = VoiceProfile {
                            name: enroll.name.clone(),
                            embedding,
                            model: model.display().to_string(),
                        };
                        save_profile(&new_profile)?;
                        profile = Some(new_profile);
                        shared.set_speaker_enabled(true, true);
                        shared.set_voice_profiles(mvx_core::list_voice_profiles());
                        shared.set_speaker_status(format!("perfil «{}» guardado", enroll.name));
                        enrollment = None;
                        let hold_ms = shared.speaker_config().1;
                        hold_until = Instant::now() + Duration::from_millis(hold_ms as u64);
                    }
                }
            }
        }

        if last_analysis.elapsed() >= analysis_interval {
            last_analysis = Instant::now();
            let (threshold, hold_ms) = shared.speaker_config();
            if analysis.len() >= WINDOW_SAMPLES {
                let window: Vec<f32> = analysis
                    .iter()
                    .skip(analysis.len() - WINDOW_SAMPLES)
                    .copied()
                    .collect();
                if let Some(current_profile) = &profile {
                    if let Some(embedding) = try_embedding(&extractor, &window) {
                        let score = cosine(&embedding, &current_profile.embedding);
                        last_score = score;
                        if score >= threshold {
                            hold_until = Instant::now() + Duration::from_millis(hold_ms as u64);
                        }
                    }
                }
            }
            let open = profile.is_some()
                && last_score >= threshold
                && (last_vad || Instant::now() <= hold_until);
            shared.set_speaker_metrics(last_score, open, last_vad);
        }

        if last_drained.elapsed() > Duration::from_secs(30) {
            last_drained = Instant::now();
            if let Some(vad) = &vad {
                vad.clear();
            }
        }
        thread::sleep(Duration::from_millis(10));
    }

    shared.set_speaker_enabled(false, false);
    shared.set_speaker_status("analizador de voz detenido");
    Ok(())
}

struct EnrollBuffer {
    name: String,
    target: usize,
    samples: Vec<f32>,
}

fn try_embedding(extractor: &SpeakerEmbeddingExtractor, samples: &[f32]) -> Option<Vec<f32>> {
    let stream = extractor.create_stream()?;
    stream.accept_waveform(ANALYSIS_RATE, samples);
    stream.input_finished();
    if !extractor.is_ready(&stream) {
        return None;
    }
    extractor.compute(&stream)
}

fn compute_embedding(extractor: &SpeakerEmbeddingExtractor, samples: &[f32]) -> Result<Vec<f32>> {
    try_embedding(extractor, samples).context("no se pudo calcular el embedding de la voz")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..len {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        0.0
    } else {
        (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0)
    }
}

fn load_profile(name: &str) -> Result<Option<VoiceProfile>> {
    let path = mvx_core::profile_path(name);
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("leyendo {}", path.display()))?;
    let profile =
        serde_json::from_str(&text).with_context(|| format!("parseando {}", path.display()))?;
    Ok(Some(profile))
}

fn save_profile(profile: &VoiceProfile) -> Result<()> {
    let dir = mvx_core::voices_dir();
    std::fs::create_dir_all(&dir)?;
    let path = mvx_core::profile_path(&profile.name);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(profile)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
