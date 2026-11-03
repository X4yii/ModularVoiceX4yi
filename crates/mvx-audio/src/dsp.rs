//! Real-time voice-processing chain.
//!
//! Everything in this module runs on the PipeWire capture thread. Buffers are
//! allocated when the path is rebuilt; `process` must not allocate, lock or log.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use deep_filter_rt::tract::DfTract;
use ndarray::Array2;
use signalsmith_stretch::Stretch;

use mvx_core::{Preset, Quality, SpeakerGate};

const SAMPLE_RATE: f32 = 48_000.0;
const MAX_CHUNK: usize = 8_192;
const CONTROL_SMOOTH_SECONDS: f32 = 0.025;
const GAIN_SMOOTH_SECONDS: f32 = 0.020;
const TONALITY_LIMIT: f32 = 8_000.0 / 48_000.0;

#[derive(Clone, Debug, Default)]
pub struct DspMetrics {
    pub input_rms: f32,
    pub output_rms: f32,
    pub gate_reduction_db: f32,
    pub comp_reduction_db: f32,
    pub spectrum: [f32; 8],
}

pub struct DspParams {
    pub pitch_semitones: f32,
    pub formant_semitones: f32,
    pub quality: Quality,
    pub noise_reduction: Option<f32>,
    pub speaker_gate: Option<SpeakerGate>,
    pub speaker_lookahead_ms: Arc<AtomicU32>,
    pub gain_db: Arc<AtomicU32>,
    pub mute: Arc<AtomicBool>,
    pub pitch_control: Arc<AtomicU32>,
    pub formant_control: Arc<AtomicU32>,
    pub noise_strength: Arc<AtomicU32>,
    pub noise_enabled: Arc<AtomicBool>,
    pub bypass: Arc<AtomicBool>,
    pub preset: Option<Preset>,
}

pub struct DspChain {
    stretch: Stretch,
    noise: DeepFilterReducer,
    gate: Option<SpeakerGate>,
    gate_delay: Vec<f32>,
    gate_pos: usize,
    gate_gain: f32,
    gate_attack: f32,
    gate_release: f32,
    scratch: Vec<f32>,
    pitch_out: Vec<f32>,
    gain_db: Arc<AtomicU32>,
    mute: Arc<AtomicBool>,
    pitch_control: Arc<AtomicU32>,
    formant_control: Arc<AtomicU32>,
    noise_enabled: Arc<AtomicBool>,
    speaker_lookahead_ms: Arc<AtomicU32>,
    bypass: Arc<AtomicBool>,
    last_pitch: f32,
    last_formant: f32,
    gain_current: f32,
    latency_ms: f32,
    // Voice Lab rack:
    high_pass: Option<Biquad>,
    noise_gate: Option<NoiseGate>,
    eq_low: Option<Biquad>,
    eq_mid: Option<Biquad>,
    eq_high: Option<Biquad>,
    compressor: Option<Compressor>,
    chorus: Option<Chorus>,
    robotizer: Option<Robotizer>,
    ring_mod: Option<RingMod>,
    distortion: Option<Distortion>,
    reverb: Option<Reverb>,
    humanizer: Option<Humanizer>,
    subsonic_filter: Biquad,
    krisp_expander: DownwardExpander,
    spectrum: SpectrumAnalyzer,
    last_metrics: DspMetrics,
}

impl DspChain {
    pub fn new(params: DspParams) -> anyhow::Result<Self> {
        let block = params.quality.block_size();
        let interval = params.quality.interval();
        let mut stretch = Stretch::new(1, block, interval);
        stretch.set_transpose_factor_semitones(params.pitch_semitones, Some(TONALITY_LIMIT));
        stretch.set_formant_factor_semitones(params.formant_semitones, false);

        let gain_db = f32::from_bits(params.gain_db.load(Ordering::Relaxed));
        let gain_current = if params.mute.load(Ordering::Relaxed) {
            0.0
        } else {
            10f32.powf(gain_db / 20.0)
        };

        let initial_noise = params.noise_reduction.unwrap_or(0.75);
        let noise = DeepFilterReducer::new(initial_noise, params.noise_strength.clone());

        // Max lookahead 300 ms (14400 samples at 48 kHz).
        let max_lookahead_samples = 300 * SAMPLE_RATE as usize / 1000;
        let gate_delay = vec![0.0; max_lookahead_samples.max(1)];

        let mut latency_ms = params.quality.latency_ms();
        if params.noise_reduction.is_some() {
            latency_ms += noise.delay_ms();
        }

        let attack = 1.0 - (-1.0 / (0.020 * SAMPLE_RATE)).exp();
        let release = 1.0 - (-1.0 / (0.140 * SAMPLE_RATE)).exp();

        let high_pass = params
            .preset
            .as_ref()
            .and_then(|p| p.high_pass())
            .map(|cutoff| Biquad::high_pass(cutoff, SAMPLE_RATE, 0.707));

        let noise_gate = params
            .preset
            .as_ref()
            .and_then(|p| p.noise_gate())
            .map(|g| NoiseGate::new(g.threshold_db, g.attack_ms, g.release_ms, SAMPLE_RATE));

        let (eq_low, eq_mid, eq_high) = if let Some((low, mid, high)) =
            params.preset.as_ref().and_then(|p| p.equalizer())
        {
            (
                Some(Biquad::low_shelf(120.0, SAMPLE_RATE, low)),
                Some(Biquad::peaking_eq(1800.0, SAMPLE_RATE, mid, 1.0)),
                Some(Biquad::high_shelf(8000.0, SAMPLE_RATE, high)),
            )
        } else {
            (None, None, None)
        };

        let compressor = params.preset.as_ref().and_then(|p| p.compressor()).map(|c| {
            Compressor::new(
                c.threshold_db,
                c.ratio,
                c.attack_ms,
                c.release_ms,
                c.makeup_db,
                SAMPLE_RATE,
            )
        });

        let chorus = params.preset.as_ref().and_then(|p| p.chorus()).map(|c| {
            Chorus::new(c.rate_hz, c.depth_ms, c.mix, SAMPLE_RATE)
        });

        let robotizer = params.preset.as_ref().and_then(|p| p.robotizer()).map(|(f, m)| {
            Robotizer::new(f, m)
        });

        let ring_mod = params.preset.as_ref().and_then(|p| p.ring_mod()).map(|(f, m)| {
            RingMod::new(f, m)
        });

        let distortion = params.preset.as_ref().and_then(|p| p.distortion()).map(|d| {
            Distortion::new(d.drive, d.crush_bits, d.mix)
        });

        let reverb = params.preset.as_ref().and_then(|p| p.reverb()).map(|r| {
            Reverb::new(r.room_size, r.damping, r.mix)
        });

        let humanizer = params.preset.as_ref().and_then(|p| p.humanizer()).map(|(d, r)| {
            Humanizer::new(d, r)
        });

        Ok(Self {
            stretch,
            noise,
            gate: params.speaker_gate,
            gate_delay,
            gate_pos: 0,
            gate_gain: 1.0,
            gate_attack: attack,
            gate_release: release,
            scratch: vec![0.0; MAX_CHUNK],
            pitch_out: vec![0.0; MAX_CHUNK],
            gain_db: params.gain_db,
            mute: params.mute,
            pitch_control: params.pitch_control,
            formant_control: params.formant_control,
            noise_enabled: params.noise_enabled,
            speaker_lookahead_ms: params.speaker_lookahead_ms,
            bypass: params.bypass,
            last_pitch: params.pitch_semitones,
            last_formant: params.formant_semitones,
            gain_current,
            latency_ms,
            high_pass,
            noise_gate,
            eq_low,
            eq_mid,
            eq_high,
            compressor,
            chorus,
            robotizer,
            ring_mod,
            distortion,
            reverb,
            humanizer,
            subsonic_filter: Biquad::high_pass(85.0, SAMPLE_RATE, 0.707),
            krisp_expander: DownwardExpander::new(-38.0, 3.0, 1.5, 60.0, 40.0, SAMPLE_RATE),
            spectrum: SpectrumAnalyzer::new(SAMPLE_RATE),
            last_metrics: DspMetrics::default(),
        })
    }

    pub fn latency_ms(&self) -> f32 {
        self.latency_ms
    }

    pub fn last_metrics(&self) -> &DspMetrics {
        &self.last_metrics
    }

    /// Update dynamic filter chain parameters in-place without restarting PipeWire.
    pub fn update_preset_filters(&mut self, preset: &Preset) {
        self.high_pass = preset
            .high_pass()
            .map(|cutoff| Biquad::high_pass(cutoff, SAMPLE_RATE, 0.707));

        self.noise_gate = preset
            .noise_gate()
            .map(|g| NoiseGate::new(g.threshold_db, g.attack_ms, g.release_ms, SAMPLE_RATE));

        let (eq_low, eq_mid, eq_high) = if let Some((low, mid, high)) = preset.equalizer() {
            (
                Some(Biquad::low_shelf(120.0, SAMPLE_RATE, low)),
                Some(Biquad::peaking_eq(1800.0, SAMPLE_RATE, mid, 1.0)),
                Some(Biquad::high_shelf(8000.0, SAMPLE_RATE, high)),
            )
        } else {
            (None, None, None)
        };
        self.eq_low = eq_low;
        self.eq_mid = eq_mid;
        self.eq_high = eq_high;

        self.compressor = preset.compressor().map(|c| {
            Compressor::new(
                c.threshold_db,
                c.ratio,
                c.attack_ms,
                c.release_ms,
                c.makeup_db,
                SAMPLE_RATE,
            )
        });

        self.chorus = preset.chorus().map(|c| {
            Chorus::new(c.rate_hz, c.depth_ms, c.mix, SAMPLE_RATE)
        });

        self.robotizer = preset.robotizer().map(|(f, m)| {
            Robotizer::new(f, m)
        });

        self.ring_mod = preset.ring_mod().map(|(f, m)| {
            RingMod::new(f, m)
        });

        self.distortion = preset.distortion().map(|d| {
            Distortion::new(d.drive, d.crush_bits, d.mix)
        });

        self.reverb = preset.reverb().map(|r| {
            Reverb::new(r.room_size, r.damping, r.mix)
        });

        self.humanizer = preset.humanizer().map(|(d, r)| {
            Humanizer::new(d, r)
        });
    }

    /// Process mono samples in place. `input` and `output` must have the same
    /// length. This is called from the realtime callback.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        let total = input.len().min(output.len());
        if total == 0 {
            return;
        }

        // 1. Input RMS and spectrum analysis
        let mut sum_sq_in = 0.0f32;
        for &s in &input[..total] {
            sum_sq_in += s * s;
        }
        let in_rms = (sum_sq_in / total as f32).sqrt();
        let spectrum_bands = self.spectrum.process_block(&input[..total]);

        let gain_db = f32::from_bits(self.gain_db.load(Ordering::Relaxed));
        let gain_target = if self.mute.load(Ordering::Relaxed) {
            0.0
        } else {
            10f32.powf(gain_db / 20.0)
        };
        smooth_toward(
            &mut self.gain_current,
            gain_target,
            total,
            GAIN_SMOOTH_SECONDS,
        );
        let gain = self.gain_current;

        let is_bypassed = self.bypass.load(Ordering::Relaxed);
        if is_bypassed {
            let mut sum_sq_out = 0.0f32;
            for (dst, src) in output[..total].iter_mut().zip(input[..total].iter()) {
                let out = soft_limit(*src * gain);
                *dst = out;
                sum_sq_out += out * out;
            }
            let out_rms = (sum_sq_out / total as f32).sqrt();
            self.last_metrics = DspMetrics {
                input_rms: in_rms,
                output_rms: out_rms,
                gate_reduction_db: 0.0,
                comp_reduction_db: 0.0,
                spectrum: spectrum_bands,
            };
            return;
        }

        let pitch_target = f32::from_bits(self.pitch_control.load(Ordering::Relaxed));
        let previous_pitch = self.last_pitch;
        smooth_toward(
            &mut self.last_pitch,
            pitch_target,
            total,
            CONTROL_SMOOTH_SECONDS,
        );

        let hum_offset = if let Some(h) = self.humanizer.as_mut() {
            h.get_pitch_offset(total, SAMPLE_RATE)
        } else {
            0.0
        };
        let effective_pitch = self.last_pitch + hum_offset;

        if (effective_pitch - previous_pitch).abs() > 0.0001 {
            self.stretch.set_transpose_factor_semitones(effective_pitch, Some(TONALITY_LIMIT));
        }

        let formant_target = f32::from_bits(self.formant_control.load(Ordering::Relaxed));
        let previous_formant = self.last_formant;
        smooth_toward(
            &mut self.last_formant,
            formant_target,
            total,
            CONTROL_SMOOTH_SECONDS,
        );
        if (self.last_formant - previous_formant).abs() > 0.0001 {
            self.stretch.set_formant_factor_semitones(self.last_formant, false);
        }

        let has_pitch_effect = effective_pitch.abs() > 0.001 || self.last_formant.abs() > 0.001;
        let noise_on = self.noise_enabled.load(Ordering::Relaxed);

        let mut max_gate_red = 0.0f32;
        let mut max_comp_red = 0.0f32;

        let mut pos = 0;
        while pos < total {
            let len = (total - pos).min(MAX_CHUNK);
            let src = &input[pos..pos + len];

            // 0. Pre-filtro subsónico dedicado (85 Hz) si la reducción de ruido está activa
            if noise_on {
                for (dst, &s) in self.scratch[..len].iter_mut().zip(src.iter()) {
                    *dst = self.subsonic_filter.process(s);
                }
            } else {
                self.scratch[..len].copy_from_slice(src);
            }

            // 1. High-Pass Filter del preset (si se configuró corte creativo más alto)
            if let Some(hpf) = self.high_pass.as_mut() {
                for s in self.scratch[..len].iter_mut() {
                    *s = hpf.process(*s);
                }
            }

            // 2. Noise Gate con histéresis
            if let Some(gate) = self.noise_gate.as_mut() {
                for s in self.scratch[..len].iter_mut() {
                    let (filtered, red_db) = gate.process_sample(*s);
                    *s = filtered;
                    if red_db > max_gate_red {
                        max_gate_red = red_db;
                    }
                }
            }

            // 3. Supresion de ruido DeepFilterNet3 (Krisp-grade)
            if noise_on {
                let mut tmp = [0.0f32; MAX_CHUNK];
                tmp[..len].copy_from_slice(&self.scratch[..len]);
                self.noise.process(&tmp[..len], &mut self.scratch[..len]);
            }

            // 4. Modulación de Tono y Formantes
            if has_pitch_effect {
                self.stretch.process(&self.scratch[..len], &mut self.pitch_out[..len]);
            } else {
                self.pitch_out[..len].copy_from_slice(&self.scratch[..len]);
            }

            // 5. Ecualizador paramétrico de 3 bandas
            if let Some(eq) = self.eq_low.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = eq.process(*s);
                }
            }
            if let Some(eq) = self.eq_mid.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = eq.process(*s);
                }
            }
            if let Some(eq) = self.eq_high.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = eq.process(*s);
                }
            }

            // 6. Compresor dinámico
            if let Some(comp) = self.compressor.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    let (compressed, red_db) = comp.process_sample(*s);
                    *s = compressed;
                    if red_db > max_comp_red {
                        max_comp_red = red_db;
                    }
                }
            }

            // 7. Efectos creativos (Character FX)
            if let Some(robot) = self.robotizer.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = robot.process_sample(*s, SAMPLE_RATE);
                }
            }
            if let Some(ring) = self.ring_mod.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = ring.process_sample(*s, SAMPLE_RATE);
                }
            }
            if let Some(dist) = self.distortion.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = dist.process_sample(*s);
                }
            }
            if let Some(chorus) = self.chorus.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = chorus.process_sample(*s, SAMPLE_RATE);
                }
            }
            if let Some(reverb) = self.reverb.as_mut() {
                for s in self.pitch_out[..len].iter_mut() {
                    *s = reverb.process_sample(*s);
                }
            }

            // 8. Speaker Gate ("Solo mi voz") y ganancia
            if self.gate.is_some() {
                let mut pitch_out = std::mem::take(&mut self.pitch_out);
                self.apply_gate(&mut pitch_out[..len], gain, &mut output[pos..pos + len]);
                self.pitch_out = pitch_out;
            } else {
                for (dst, src) in output[pos..pos + len]
                    .iter_mut()
                    .zip(self.pitch_out[..len].iter())
                {
                    *dst = *src * gain;
                }
            }

            // 9. Expansor Descendente Inteligente estilo Krisp:
            // Elimina el siseo residual y ruido de fondo en reposo / pausas cuando la reducción de ruido está activa.
            if noise_on {
                for s in output[pos..pos + len].iter_mut() {
                    let (cleaned, red_db) = self.krisp_expander.process_sample(*s);
                    *s = cleaned;
                    if red_db > max_gate_red {
                        max_gate_red = red_db;
                    }
                }
            }
            pos += len;
        }

        let mut sum_sq_out = 0.0f32;
        for sample in &mut output[..total] {
            *sample = soft_limit(*sample);
            sum_sq_out += *sample * *sample;
        }
        let out_rms = (sum_sq_out / total as f32).sqrt();

        self.last_metrics = DspMetrics {
            input_rms: in_rms,
            output_rms: out_rms,
            gate_reduction_db: max_gate_red,
            comp_reduction_db: max_comp_red,
            spectrum: spectrum_bands,
        };
    }

    fn apply_gate(&mut self, samples: &mut [f32], gain: f32, output: &mut [f32]) {
        let gate = self.gate.as_ref().expect("gate missing");
        let enabled = gate.enabled.load(Ordering::Relaxed);
        let available = gate.available.load(Ordering::Relaxed);
        if !enabled || !available {
            self.gate_gain = 1.0;
            for (dst, src) in output.iter_mut().zip(samples.iter()) {
                *dst = *src * gain;
            }
            return;
        }
        let target = gate.target.load(Ordering::Relaxed);
        let target_gain = if target { 1.0 } else { 0.0 };
        let coeff = if target_gain > self.gate_gain {
            self.gate_attack
        } else {
            self.gate_release
        };

        let lookahead_ms = self.speaker_lookahead_ms.load(Ordering::Relaxed).min(300);
        let delay_len = (lookahead_ms as usize * SAMPLE_RATE as usize / 1000).max(1);

        for (index, sample) in samples.iter_mut().enumerate() {
            let current = *sample;
            let read = self.gate_pos % delay_len;
            let delayed = self.gate_delay[read];
            self.gate_delay[read] = current;
            self.gate_pos = (read + 1) % delay_len;
            self.gate_gain += (target_gain - self.gate_gain) * coeff;
            output[index] = delayed * gain * self.gate_gain;
        }
    }
}

const DF_HOP_SIZE: usize = 480;

/// DeepFilterNet3 real-time speech enhancement engine.
/// Runs full deep-filtering neural network on CPU with embedded weights.
struct DeepFilterReducer {
    df: DfTract,
    input_buf: [f32; DF_HOP_SIZE],
    input_pos: usize,
    queue: Vec<f32>,
    queue_read: usize,
    queue_write: usize,
    queue_len: usize,
    noisy_view: Array2<f32>,
    enh_view: Array2<f32>,
    strength: Arc<AtomicU32>,
    delay_samples: usize,
    last_lsnr: f32,
}

impl DeepFilterReducer {
    fn new(strength: f32, control: Arc<AtomicU32>) -> Self {
        let df = DfTract::default();
        let delay_samples = df.delay();
        let queue_capacity = delay_samples + DF_HOP_SIZE * 4;
        let queue = vec![0.0f32; queue_capacity];

        // Pre-fill queue with delay silence to maintain 1:1 steady-state stream
        let queue_len = delay_samples;
        let queue_write = delay_samples % queue_capacity;

        control.store(strength.to_bits(), Ordering::Relaxed);

        Self {
            df,
            input_buf: [0.0f32; DF_HOP_SIZE],
            input_pos: 0,
            queue,
            queue_read: 0,
            queue_write,
            queue_len,
            noisy_view: Array2::<f32>::zeros((1, DF_HOP_SIZE)),
            enh_view: Array2::<f32>::zeros((1, DF_HOP_SIZE)),
            strength: control,
            delay_samples,
            last_lsnr: 0.0,
        }
    }

    pub fn delay_ms(&self) -> f32 {
        self.delay_samples as f32 * 1000.0 / SAMPLE_RATE
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let strength = f32::from_bits(self.strength.load(Ordering::Relaxed)).clamp(0.0, 1.0);
        let max_atten_db = 15.0 + 85.0 * strength;
        self.df.set_atten_lim(max_atten_db);

        for (i, &sample) in input.iter().enumerate() {
            output[i] = self.pop();
            self.input_buf[self.input_pos] = sample;
            self.input_pos += 1;

            if self.input_pos == DF_HOP_SIZE {
                self.input_pos = 0;
                for idx in 0..DF_HOP_SIZE {
                    self.noisy_view[[0, idx]] = self.input_buf[idx];
                }

                if let Ok(lsnr) = self.df.process(self.noisy_view.view(), self.enh_view.view_mut()) {
                    self.last_lsnr = lsnr;
                    for idx in 0..DF_HOP_SIZE {
                        let clean = self.enh_view[[0, idx]];
                        self.push(clean);
                    }
                } else {
                    for idx in 0..DF_HOP_SIZE {
                        self.push(self.input_buf[idx]);
                    }
                }
            }
        }
    }

    fn push(&mut self, sample: f32) {
        if self.queue_len == self.queue.len() {
            return;
        }
        self.queue[self.queue_write] = sample;
        self.queue_write = (self.queue_write + 1) % self.queue.len();
        self.queue_len += 1;
    }

    fn pop(&mut self) -> f32 {
        if self.queue_len == 0 {
            return 0.0;
        }
        let sample = self.queue[self.queue_read];
        self.queue_read = (self.queue_read + 1) % self.queue.len();
        self.queue_len -= 1;
        sample
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    s1: f32,
    s2: f32,
}

impl Biquad {
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    pub fn high_pass(cutoff_hz: f32, sample_rate: f32, q: f32) -> Self {
        let omega = 2.0 * std::f32::consts::PI * (cutoff_hz.max(10.0) / sample_rate).min(0.45);
        let alpha = omega.sin() / (2.0 * q.max(0.1));
        let cos_w = omega.cos();

        let b0 = (1.0 + cos_w) / 2.0;
        let b1 = -(1.0 + cos_w);
        let b2 = (1.0 + cos_w) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    pub fn band_pass(center_hz: f32, sample_rate: f32, q: f32) -> Self {
        let omega = 2.0 * std::f32::consts::PI * (center_hz.max(10.0) / sample_rate).min(0.48);
        let alpha = omega.sin() / (2.0 * q.max(0.1));
        let cos_w = omega.cos();

        let b0 = alpha;
        let b1 = 0.0;
        let b2 = -alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    pub fn low_shelf(cutoff_hz: f32, sample_rate: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.05 {
            return Self::identity();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let omega = 2.0 * std::f32::consts::PI * (cutoff_hz.max(10.0) / sample_rate).min(0.45);
        let cos_w = omega.cos();
        let sin_w = omega.sin();
        let alpha = sin_w * a.sqrt();

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w + 2.0 * a.sqrt() * alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w - 2.0 * a.sqrt() * alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w + 2.0 * a.sqrt() * alpha;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w);
        let a2 = (a + 1.0) + (a - 1.0) * cos_w - 2.0 * a.sqrt() * alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    pub fn peaking_eq(center_hz: f32, sample_rate: f32, gain_db: f32, q: f32) -> Self {
        if gain_db.abs() < 0.05 {
            return Self::identity();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let omega = 2.0 * std::f32::consts::PI * (center_hz.max(20.0) / sample_rate).min(0.45);
        let alpha = omega.sin() / (2.0 * q.max(0.1));
        let cos_w = omega.cos();

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    pub fn high_shelf(cutoff_hz: f32, sample_rate: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.05 {
            return Self::identity();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let omega = 2.0 * std::f32::consts::PI * (cutoff_hz.max(100.0) / sample_rate).min(0.45);
        let cos_w = omega.cos();
        let sin_w = omega.sin();
        let alpha = sin_w * a.sqrt();

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w + 2.0 * a.sqrt() * alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w - 2.0 * a.sqrt() * alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w + 2.0 * a.sqrt() * alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w);
        let a2 = (a + 1.0) - (a - 1.0) * cos_w - 2.0 * a.sqrt() * alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }
}

pub struct NoiseGate {
    threshold_linear: f32,
    attack_coeff: f32,
    release_coeff: f32,
    env: f32,
    gain: f32,
}

impl NoiseGate {
    pub fn new(threshold_db: f32, attack_ms: f32, release_ms: f32, sample_rate: f32) -> Self {
        let threshold_linear = 10f32.powf(threshold_db / 20.0);
        let attack_coeff = 1.0 - (-1.0 / (attack_ms.max(0.1) * 0.001 * sample_rate)).exp();
        let release_coeff = 1.0 - (-1.0 / (release_ms.max(1.0) * 0.001 * sample_rate)).exp();
        Self {
            threshold_linear,
            attack_coeff,
            release_coeff,
            env: 0.0,
            gain: 0.0,
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32) -> (f32, f32) {
        let abs = sample.abs();
        let coeff = if abs > self.env {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.env += (abs - self.env) * coeff;

        let close_threshold = self.threshold_linear * 0.707;
        let target_gain = if self.env > self.threshold_linear {
            1.0
        } else if self.env < close_threshold {
            0.0
        } else {
            self.gain
        };

        let g_coeff = if target_gain > self.gain {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.gain += (target_gain - self.gain) * g_coeff;

        let reduction_db = if self.gain < 0.999 {
            20.0 * (self.gain.max(1e-4)).log10().abs()
        } else {
            0.0
        };

        (sample * self.gain, reduction_db)
    }
}

/// Downward expander for smooth, natural noise-floor suppression (Krisp-style).
/// Unlike a hard gate, it attenuates progressively below threshold without clicks or clipping words.
pub struct DownwardExpander {
    threshold_db: f32,
    ratio: f32,
    attack_coeff: f32,
    release_coeff: f32,
    max_attenuation_db: f32,
    env: f32,
    gain: f32,
}

impl DownwardExpander {
    pub fn new(
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        max_attenuation_db: f32,
        sample_rate: f32,
    ) -> Self {
        let attack_coeff = 1.0 - (-1.0 / (attack_ms.max(0.1) * 0.001 * sample_rate)).exp();
        let release_coeff = 1.0 - (-1.0 / (release_ms.max(1.0) * 0.001 * sample_rate)).exp();
        Self {
            threshold_db,
            ratio: ratio.max(1.0),
            attack_coeff,
            release_coeff,
            max_attenuation_db: max_attenuation_db.abs(),
            env: 0.0,
            gain: 1.0,
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32) -> (f32, f32) {
        let abs = sample.abs();
        let coeff = if abs > self.env {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.env += (abs - self.env) * coeff;

        let env_db = 20.0 * (self.env.max(1e-5)).log10();
        let target_gain = if env_db < self.threshold_db {
            let under_db = self.threshold_db - env_db;
            let atten_db = (under_db * (self.ratio - 1.0)).min(self.max_attenuation_db);
            10f32.powf(-atten_db / 20.0)
        } else {
            1.0
        };

        let g_coeff = if target_gain > self.gain {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.gain += (target_gain - self.gain) * g_coeff;

        let reduction_db = 20.0 * (self.gain.max(1e-4)).log10().abs();
        (sample * self.gain, reduction_db)
    }
}

pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack_coeff: f32,
    release_coeff: f32,
    makeup_linear: f32,
    env: f32,
    gain_linear: f32,
}

impl Compressor {
    pub fn new(
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
        sample_rate: f32,
    ) -> Self {
        let attack_coeff = 1.0 - (-1.0 / (attack_ms.max(0.1) * 0.001 * sample_rate)).exp();
        let release_coeff = 1.0 - (-1.0 / (release_ms.max(1.0) * 0.001 * sample_rate)).exp();
        let makeup_linear = 10f32.powf(makeup_db / 20.0);
        Self {
            threshold_db,
            ratio: ratio.max(1.0),
            attack_coeff,
            release_coeff,
            makeup_linear,
            env: 0.0,
            gain_linear: 1.0,
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32) -> (f32, f32) {
        let abs = sample.abs();
        let coeff = if abs > self.env {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.env += (abs - self.env) * coeff;

        let env_db = 20.0 * (self.env.max(1e-5)).log10();
        let target_reduction_db = if env_db > self.threshold_db {
            (env_db - self.threshold_db) * (1.0 - 1.0 / self.ratio)
        } else {
            0.0
        };

        let target_gain = 10f32.powf(-target_reduction_db / 20.0);
        let g_coeff = if target_gain < self.gain_linear {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.gain_linear += (target_gain - self.gain_linear) * g_coeff;

        let out = sample * self.gain_linear * self.makeup_linear;
        (out, target_reduction_db)
    }
}

const CHORUS_MAX_DELAY: usize = 2400;

pub struct Chorus {
    buffer: Vec<f32>,
    write_pos: usize,
    lfo_phase: f32,
    rate_hz: f32,
    depth_samples: f32,
    mix: f32,
}

impl Chorus {
    pub fn new(rate_hz: f32, depth_ms: f32, mix: f32, sample_rate: f32) -> Self {
        let depth_samples = (depth_ms * 0.001 * sample_rate).min(CHORUS_MAX_DELAY as f32 / 2.0);
        Self {
            buffer: vec![0.0; CHORUS_MAX_DELAY],
            write_pos: 0,
            lfo_phase: 0.0,
            rate_hz,
            depth_samples,
            mix: mix.clamp(0.0, 1.0),
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32, sample_rate: f32) -> f32 {
        if self.mix < 0.01 {
            return sample;
        }
        self.buffer[self.write_pos] = sample;

        let lfo = (self.lfo_phase * 2.0 * std::f32::consts::PI).sin();
        self.lfo_phase += self.rate_hz / sample_rate;
        if self.lfo_phase >= 1.0 {
            self.lfo_phase -= 1.0;
        }

        let base_delay = self.depth_samples * 1.5;
        let mod_delay = base_delay + self.depth_samples * 0.5 * lfo;
        let read_f = (self.write_pos as f32 + CHORUS_MAX_DELAY as f32 - mod_delay)
            % (CHORUS_MAX_DELAY as f32);
        let i0 = read_f.floor() as usize % CHORUS_MAX_DELAY;
        let i1 = (i0 + 1) % CHORUS_MAX_DELAY;
        let frac = read_f - read_f.floor();

        let delayed = self.buffer[i0] + frac * (self.buffer[i1] - self.buffer[i0]);
        self.write_pos = (self.write_pos + 1) % CHORUS_MAX_DELAY;

        sample * (1.0 - self.mix * 0.5) + delayed * (self.mix * 0.7)
    }
}

pub struct Robotizer {
    frequency_hz: f32,
    phase: f32,
    mix: f32,
}

impl Robotizer {
    pub fn new(frequency_hz: f32, mix: f32) -> Self {
        Self {
            frequency_hz,
            phase: 0.0,
            mix: mix.clamp(0.0, 1.0),
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32, sample_rate: f32) -> f32 {
        if self.mix < 0.01 {
            return sample;
        }
        let carrier = if self.phase < 0.5 {
            4.0 * self.phase - 1.0
        } else {
            3.0 - 4.0 * self.phase
        };

        self.phase += self.frequency_hz / sample_rate;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }

        let wet = sample.abs() * carrier;
        sample * (1.0 - self.mix) + wet * self.mix
    }
}

pub struct RingMod {
    frequency_hz: f32,
    phase: f32,
    mix: f32,
}

impl RingMod {
    pub fn new(frequency_hz: f32, mix: f32) -> Self {
        Self {
            frequency_hz,
            phase: 0.0,
            mix: mix.clamp(0.0, 1.0),
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32, sample_rate: f32) -> f32 {
        if self.mix < 0.01 {
            return sample;
        }
        let carrier = (self.phase * 2.0 * std::f32::consts::PI).sin();
        self.phase += self.frequency_hz / sample_rate;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }

        let wet = sample * carrier;
        sample * (1.0 - self.mix) + wet * self.mix
    }
}

pub struct Distortion {
    drive: f32,
    crush_bits: u32,
    mix: f32,
}

impl Distortion {
    pub fn new(drive: f32, crush_bits: u32, mix: f32) -> Self {
        Self {
            drive: drive.clamp(1.0, 30.0),
            crush_bits: crush_bits.min(16),
            mix: mix.clamp(0.0, 1.0),
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32) -> f32 {
        if self.mix < 0.01 {
            return sample;
        }
        let x = sample * self.drive;
        let mut wet = (x / (1.0 + x.abs())).clamp(-1.0, 1.0);

        if self.crush_bits > 0 && self.crush_bits <= 16 {
            let levels = (1u32 << (self.crush_bits - 1)) as f32;
            wet = (wet * levels).round() / levels;
        }

        sample * (1.0 - self.mix) + wet * self.mix
    }
}

struct CombFilter {
    buffer: Vec<f32>,
    pos: usize,
    filter_store: f32,
    feedback: f32,
    damp: f32,
}

impl CombFilter {
    fn new(size: usize, feedback: f32, damp: f32) -> Self {
        Self {
            buffer: vec![0.0; size],
            pos: 0,
            filter_store: 0.0,
            feedback,
            damp,
        }
    }

    #[inline(always)]
    fn process(&mut self, input: f32) -> f32 {
        let output = self.buffer[self.pos];
        self.filter_store = output * (1.0 - self.damp) + self.filter_store * self.damp;
        self.buffer[self.pos] = input + self.filter_store * self.feedback;
        self.pos = (self.pos + 1) % self.buffer.len();
        output
    }
}

struct AllpassFilter {
    buffer: Vec<f32>,
    pos: usize,
    feedback: f32,
}

impl AllpassFilter {
    fn new(size: usize, feedback: f32) -> Self {
        Self {
            buffer: vec![0.0; size],
            pos: 0,
            feedback,
        }
    }

    #[inline(always)]
    fn process(&mut self, input: f32) -> f32 {
        let buf_out = self.buffer[self.pos];
        let output = -input + buf_out;
        self.buffer[self.pos] = input + (buf_out * self.feedback);
        self.pos = (self.pos + 1) % self.buffer.len();
        output
    }
}

pub struct Reverb {
    combs: [CombFilter; 4],
    allpasses: [AllpassFilter; 2],
    mix: f32,
}

impl Reverb {
    pub fn new(room_size: f32, damping: f32, mix: f32) -> Self {
        let feedback = (room_size.clamp(0.0, 1.0) * 0.28 + 0.7).min(0.98);
        let damp = damping.clamp(0.0, 1.0) * 0.4;
        let combs = [
            CombFilter::new(1116, feedback, damp),
            CombFilter::new(1188, feedback, damp),
            CombFilter::new(1277, feedback, damp),
            CombFilter::new(1356, feedback, damp),
        ];
        let allpasses = [
            AllpassFilter::new(225, 0.5),
            AllpassFilter::new(556, 0.5),
        ];
        Self {
            combs,
            allpasses,
            mix: mix.clamp(0.0, 1.0),
        }
    }

    #[inline(always)]
    pub fn process_sample(&mut self, sample: f32) -> f32 {
        if self.mix < 0.01 {
            return sample;
        }
        let mut out = 0.0f32;
        for comb in &mut self.combs {
            out += comb.process(sample);
        }
        for allpass in &mut self.allpasses {
            out = allpass.process(out);
        }
        sample * (1.0 - self.mix * 0.5) + out * (self.mix * 0.25)
    }
}

pub struct Humanizer {
    depth_semitones: f32,
    rate_hz: f32,
    phase: f32,
}

impl Humanizer {
    pub fn new(depth_cents: f32, rate_hz: f32) -> Self {
        Self {
            depth_semitones: depth_cents * 0.01,
            rate_hz,
            phase: 0.0,
        }
    }

    #[inline(always)]
    pub fn get_pitch_offset(&mut self, frames: usize, sample_rate: f32) -> f32 {
        if self.depth_semitones < 0.001 {
            return 0.0;
        }
        let offset = (self.phase * 2.0 * std::f32::consts::PI).sin() * self.depth_semitones;
        self.phase += (self.rate_hz * frames as f32) / sample_rate;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        offset
    }
}

pub struct SpectrumAnalyzer {
    filters: [Biquad; 8],
    envelopes: [f32; 8],
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: f32) -> Self {
        let freqs = [60.0, 150.0, 400.0, 1000.0, 2400.0, 5000.0, 9000.0, 15000.0];
        let filters = std::array::from_fn(|i| Biquad::band_pass(freqs[i], sample_rate, 1.4));
        Self {
            filters,
            envelopes: [-60.0; 8],
        }
    }

    pub fn process_block(&mut self, samples: &[f32]) -> [f32; 8] {
        if samples.is_empty() {
            return self.envelopes;
        }
        let mut powers = [0.0f32; 8];
        for &s in samples {
            for i in 0..8 {
                let bp = self.filters[i].process(s);
                powers[i] += bp * bp;
            }
        }
        let inv_len = 1.0 / samples.len() as f32;
        let smooth = 0.35f32;
        for i in 0..8 {
            let rms = (powers[i] * inv_len).sqrt();
            let db = if rms > 1e-4 {
                (20.0 * rms.log10()).clamp(-60.0, 0.0)
            } else {
                -60.0
            };
            self.envelopes[i] = self.envelopes[i] * (1.0 - smooth) + db * smooth;
        }
        self.envelopes
    }
}

/// Move a control value toward its target with an exponential time constant.
/// Slider updates arrive in small steps; feeding those steps straight into the
/// phase vocoder causes audible zipper noise and warbling.
fn smooth_toward(current: &mut f32, target: f32, frames: usize, seconds: f32) {
    if frames == 0 || (*current - target).abs() <= f32::EPSILON {
        *current = target;
        return;
    }
    let coeff = 1.0 - (-(frames as f32) / (seconds * SAMPLE_RATE)).exp();
    *current += (target - *current) * coeff;
    if (target - *current).abs() < 0.0001 {
        *current = target;
    }
}

/// Linear below 0.95, then a smooth knee up to 1.0. This is deliberately
/// cheap: it is a safety net, not a loudness maximizer.
fn soft_limit(sample: f32) -> f32 {
    let abs = sample.abs();
    if abs <= 0.95 {
        sample
    } else {
        let sign = sample.signum();
        let knee = (abs - 0.95) / 0.05;
        sign * (0.95 + 0.05 * (knee / (1.0 + knee)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_filter_reducer_has_delay_latency_and_same_length() {
        let mut reducer = DeepFilterReducer::new(1.0, Arc::new(AtomicU32::new(1.0f32.to_bits())));
        let delay = reducer.delay_samples;
        let input = vec![0.25; DF_HOP_SIZE * 6];
        let mut output = vec![0.0; input.len()];
        reducer.process(&input, &mut output);
        assert_eq!(output.len(), input.len());
        assert!(output[..delay].iter().all(|v| *v == 0.0));
        assert!(output[delay..].iter().any(|v| *v != 0.0));
    }

    #[test]
    fn limiter_never_exceeds_one() {
        for sample in [-8.0, -1.0, 0.0, 1.0, 8.0] {
            assert!(soft_limit(sample).abs() <= 1.0);
        }
    }

    #[test]
    fn bypass_without_pitch_or_formant_is_transparent() {
        let gain = std::sync::Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let mute = std::sync::Arc::new(AtomicBool::new(false));
        let mut chain = DspChain::new(DspParams {
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Ultra,
            noise_reduction: None,
            speaker_gate: None,
            speaker_lookahead_ms: Arc::new(AtomicU32::new(0)),
            gain_db: gain,
            mute,
            pitch_control: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            formant_control: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            noise_strength: Arc::new(AtomicU32::new(0.75f32.to_bits())),
            noise_enabled: Arc::new(AtomicBool::new(false)),
            bypass: Arc::new(AtomicBool::new(false)),
            preset: None,
        })
        .unwrap();
        let input: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
        let mut output = vec![0.0; input.len()];
        chain.process(&input, &mut output);
        for (a, b) in input.iter().zip(output.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn live_pitch_control_glides_instead_of_jumping() {
        let gain = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let mute = Arc::new(AtomicBool::new(false));
        let pitch = Arc::new(AtomicU32::new(6.0f32.to_bits()));
        let mut chain = DspChain::new(DspParams {
            pitch_semitones: 6.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            noise_reduction: None,
            speaker_gate: None,
            speaker_lookahead_ms: Arc::new(AtomicU32::new(0)),
            gain_db: gain,
            mute,
            pitch_control: pitch.clone(),
            formant_control: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            noise_strength: Arc::new(AtomicU32::new(0.75f32.to_bits())),
            noise_enabled: Arc::new(AtomicBool::new(false)),
            bypass: Arc::new(AtomicBool::new(false)),
            preset: None,
        })
        .unwrap();
        let input = vec![0.0; 256];
        let mut output = vec![0.0; input.len()];

        pitch.store(0.0f32.to_bits(), Ordering::Relaxed);
        chain.process(&input, &mut output);
        assert!(chain.last_pitch > 0.0 && chain.last_pitch < 6.0);

        for _ in 0..40 {
            chain.process(&input, &mut output);
        }
        assert!(chain.last_pitch.abs() < 0.001);
    }

    #[test]
    fn gain_changes_are_smoothed() {
        let gain = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let mute = Arc::new(AtomicBool::new(false));
        let mut chain = DspChain::new(DspParams {
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            quality: Quality::Balanced,
            noise_reduction: None,
            speaker_gate: None,
            speaker_lookahead_ms: Arc::new(AtomicU32::new(0)),
            gain_db: gain.clone(),
            mute,
            pitch_control: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            formant_control: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            noise_strength: Arc::new(AtomicU32::new(0.75f32.to_bits())),
            noise_enabled: Arc::new(AtomicBool::new(false)),
            bypass: Arc::new(AtomicBool::new(false)),
            preset: None,
        })
        .unwrap();
        let input = vec![0.25; 256];
        let mut output = vec![0.0; input.len()];

        gain.store((-20.0f32).to_bits(), Ordering::Relaxed);
        chain.process(&input, &mut output);
        assert!(output[0] > 0.15, "first block should not jump to -20 dB");

        for _ in 0..40 {
            chain.process(&input, &mut output);
        }
        assert!((output[0] - 0.025).abs() < 0.002);
    }

    #[test]
    fn octave_up_shift_changes_sine_frequency() {
        let gain = std::sync::Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let mute = std::sync::Arc::new(AtomicBool::new(false));
        let mut chain = DspChain::new(DspParams {
            pitch_semitones: 12.0,
            formant_semitones: 0.0,
            quality: Quality::Ultra,
            noise_reduction: None,
            speaker_gate: None,
            speaker_lookahead_ms: Arc::new(AtomicU32::new(0)),
            gain_db: gain,
            mute,
            pitch_control: Arc::new(AtomicU32::new(12.0f32.to_bits())),
            formant_control: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            noise_strength: Arc::new(AtomicU32::new(0.75f32.to_bits())),
            noise_enabled: Arc::new(AtomicBool::new(false)),
            bypass: Arc::new(AtomicBool::new(false)),
            preset: None,
        })
        .unwrap();
        let sample_rate = 48_000.0f32;
        let input: Vec<f32> = (0..96_000)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate).sin() * 0.25)
            .collect();
        let mut output = vec![0.0; input.len()];
        for (index, chunk) in input.chunks(256).enumerate() {
            let start = index * 256;
            let end = (start + chunk.len()).min(output.len());
            chain.process(chunk, &mut output[start..end]);
        }
        let start = 24_000;
        let mut crossings = 0usize;
        for pair in output[start..].windows(2) {
            if pair[0] <= 0.0 && pair[1] > 0.0 {
                crossings += 1;
            }
        }
        let seconds = (output.len() - start) as f32 / sample_rate;
        let frequency = crossings as f32 / seconds;
        assert!(
            (frequency - 880.0).abs() < 30.0,
            "expected ~880 Hz, got {frequency} Hz"
        );
    }

    #[test]
    fn high_pass_filter_attenuates_low_frequencies() {
        let mut hpf = Biquad::high_pass(200.0, SAMPLE_RATE, 0.707);
        // 30 Hz tone should be heavily attenuated
        let mut low_peak = 0.0f32;
        for i in 0..1000 {
            let s = (2.0 * std::f32::consts::PI * 30.0 * i as f32 / SAMPLE_RATE).sin();
            let out = hpf.process(s);
            if i > 200 {
                low_peak = low_peak.max(out.abs());
            }
        }
        assert!(low_peak < 0.2, "30 Hz should be attenuated, got {low_peak}");

        // 1000 Hz tone should pass through nearly unattenuated
        let mut mid_peak = 0.0f32;
        for i in 0..1000 {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SAMPLE_RATE).sin();
            let out = hpf.process(s);
            if i > 200 {
                mid_peak = mid_peak.max(out.abs());
            }
        }
        assert!(mid_peak > 0.9, "1000 Hz should pass through, got {mid_peak}");
    }

    #[test]
    fn noise_gate_attenuates_silence() {
        let mut gate = NoiseGate::new(-30.0, 5.0, 50.0, SAMPLE_RATE);
        // Feed silence/noise at -50 dB (amplitude ~0.003)
        let mut out_val = 0.0;
        for _ in 0..1000 {
            let (out, red) = gate.process_sample(0.003);
            out_val = out;
            let _ = red;
        }
        assert!(out_val < 0.0005, "gate should stay closed on sub-threshold signal");

        // Feed strong signal at 0 dB (amplitude 0.5)
        for _ in 0..2000 {
            let (out, _) = gate.process_sample(0.5);
            out_val = out;
        }
        assert!((out_val - 0.5).abs() < 0.05, "gate should open on strong signal");

        // Feed silence again for 10000 samples (~200ms) to ensure release closes
        for _ in 0..10000 {
            let (out, _) = gate.process_sample(0.0001);
            out_val = out;
        }
        assert!(out_val < 0.00005, "gate should close again after release");
    }

    #[test]
    fn compressor_reduces_loud_signals() {
        let mut comp = Compressor::new(-12.0, 4.0, 5.0, 50.0, 0.0, SAMPLE_RATE);
        // Threshold -12 dB is amplitude ~0.25. Feed 0.8
        let mut last_out = 0.0;
        let mut last_red = 0.0;
        for _ in 0..2000 {
            let (out, red) = comp.process_sample(0.8);
            last_out = out;
            last_red = red;
        }
        assert!(last_out < 0.6, "loud signal should be compressed");
        assert!(last_red > 2.0, "reduction dB should be reported");
    }

    #[test]
    fn distortion_saturates_properly() {
        let mut dist = Distortion::new(5.0, 0, 1.0);
        let out = dist.process_sample(0.5);
        // Without distortion, 0.5; with drive 5.0, x=2.5, saturated < 1.0
        assert!(out > 0.6 && out <= 1.0);
    }

    #[test]
    fn downward_expander_cleans_noise_floor() {
        let mut expander = DownwardExpander::new(-38.0, 3.0, 1.5, 60.0, 40.0, SAMPLE_RATE);

        // Strong speech signal at -15 dB (amp ~0.18) should pass unattenuated (gain = 1.0)
        let mut last_gain = 0.0f32;
        for _ in 0..2000 {
            let (out, red) = expander.process_sample(0.18);
            last_gain = out / 0.18;
            let _ = red;
        }
        assert!((last_gain - 1.0).abs() < 0.05, "speech signal should pass transparently");

        // Noise floor at -50 dB (amp ~0.003) should be heavily attenuated
        let mut noise_out = 0.0f32;
        let mut noise_red = 0.0f32;
        for _ in 0..30000 {
            let (out, red) = expander.process_sample(0.003);
            noise_out = out;
            noise_red = red;
        }
        assert!(noise_red > 15.0, "noise floor should suffer downward expansion (>15 dB reduction, got {noise_red})");
        assert!(noise_out < 0.001, "noise floor amplitude should drop significantly");
    }

    #[test]
    fn deep_filter_net_inference_runs() {
        use deep_filter_rt::tract::DfTract;
        use ndarray::Array2;

        let mut df = DfTract::default();
        let hop = df.hop_size;
        let mut noisy = Array2::<f32>::zeros((1, hop));
        let mut enh = Array2::<f32>::zeros((1, hop));

        for i in 0..hop {
            noisy[[0, i]] = (i as f32 * 0.1).sin() * 0.4 + 0.02;
        }

        let lsnr = df.process(noisy.view(), enh.view_mut()).unwrap();
        assert!(lsnr.is_finite(), "lsnr should be finite, got {lsnr}");
    }
}
