//! Real-time voice-processing chain.
//!
//! Everything in this module runs on the PipeWire capture thread. Buffers are
//! allocated when the path is rebuilt; `process` must not allocate, lock or log.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use nnnoiseless::{DenoiseState, FRAME_SIZE};
use signalsmith_stretch::Stretch;

use mvx_core::{Quality, SpeakerGate};

const SAMPLE_RATE: f32 = 48_000.0;
const MAX_CHUNK: usize = 8_192;
const NOISE_FRAME: usize = FRAME_SIZE;
const CONTROL_SMOOTH_SECONDS: f32 = 0.025;
const GAIN_SMOOTH_SECONDS: f32 = 0.020;
const TONALITY_LIMIT: f32 = 8_000.0 / 48_000.0;

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
}

pub struct DspChain {
    stretch: Stretch,
    noise: NoiseReducer,
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
}

impl DspChain {
    pub fn new(params: DspParams) -> anyhow::Result<Self> {
        let block = params.quality.block_size();
        let interval = params.quality.interval();
        let mut stretch = Stretch::new(1, block, interval);
        stretch.set_transpose_factor_semitones(params.pitch_semitones, Some(TONALITY_LIMIT));
        stretch.set_formant_factor_semitones(params.formant_semitones, true);

        let gain_db = f32::from_bits(params.gain_db.load(Ordering::Relaxed));
        let gain_current = if params.mute.load(Ordering::Relaxed) {
            0.0
        } else {
            10f32.powf(gain_db / 20.0)
        };

        let initial_noise = params.noise_reduction.unwrap_or(0.75);
        let noise = NoiseReducer::new(initial_noise, params.noise_strength.clone());

        // Max lookahead 300 ms (14400 samples at 48 kHz).
        let max_lookahead_samples = 300 * SAMPLE_RATE as usize / 1000;
        let gate_delay = vec![0.0; max_lookahead_samples.max(1)];

        let mut latency_ms = params.quality.latency_ms();
        if params.noise_reduction.is_some() {
            latency_ms += NOISE_FRAME as f32 * 1000.0 / SAMPLE_RATE;
        }

        // A 20 ms attack and a 140 ms release keep word onsets intact while
        // making the gate feel natural when another voice takes over.
        let attack = 1.0 - (-1.0 / (0.020 * SAMPLE_RATE)).exp();
        let release = 1.0 - (-1.0 / (0.140 * SAMPLE_RATE)).exp();

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
        })
    }

    pub fn latency_ms(&self) -> f32 {
        self.latency_ms
    }

    /// Process mono samples in place. `input` and `output` must have the same
    /// length. This is called from the realtime callback.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        let total = input.len().min(output.len());
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
            for (dst, src) in output[..total].iter_mut().zip(input[..total].iter()) {
                *dst = soft_limit(*src * gain);
            }
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
        if (self.last_pitch - previous_pitch).abs() > 0.0001 {
            self.stretch.set_transpose_factor_semitones(self.last_pitch, Some(TONALITY_LIMIT));
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
            self.stretch.set_formant_factor_semitones(self.last_formant, true);
        }

        let has_pitch_effect = self.last_pitch.abs() > 0.001 || self.last_formant.abs() > 0.001;
        let noise_on = self.noise_enabled.load(Ordering::Relaxed);

        let mut pos = 0;
        while pos < total {
            let len = (total - pos).min(MAX_CHUNK);
            let src = &input[pos..pos + len];

            if noise_on {
                self.noise.process(src, &mut self.scratch[..len]);
            } else {
                self.scratch[..len].copy_from_slice(src);
            }

            if has_pitch_effect {
                self.stretch.process(&self.scratch[..len], &mut self.pitch_out[..len]);
            } else {
                self.pitch_out[..len].copy_from_slice(&self.scratch[..len]);
            }

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
            pos += len;
        }

        for sample in &mut output[..total] {
            *sample = soft_limit(*sample);
        }
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

/// RNNoise works with 480-sample frames and expects i16-scale floats.
/// The wrapper delays by one frame so the returned stream has the same length
/// as the input stream.
struct NoiseReducer {
    state: Box<DenoiseState<'static>>,
    input: [f32; NOISE_FRAME],
    input_pos: usize,
    queue: [f32; NOISE_FRAME * 2],
    queue_read: usize,
    queue_write: usize,
    queue_len: usize,
    strength: Arc<AtomicU32>,
    current_strength: f32,
    wet: [f32; NOISE_FRAME],
}

impl NoiseReducer {
    fn new(strength: f32, control: Arc<AtomicU32>) -> Self {
        let strength = strength.clamp(0.0, 1.0);
        control.store(strength.to_bits(), Ordering::Relaxed);
        Self {
            state: DenoiseState::new(),
            input: [0.0; NOISE_FRAME],
            input_pos: 0,
            queue: [0.0; NOISE_FRAME * 2],
            queue_read: 0,
            queue_write: 0,
            queue_len: 0,
            strength: control,
            current_strength: strength,
            wet: [0.0; NOISE_FRAME],
        }
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        for (i, sample) in input.iter().copied().enumerate() {
            // Pop before accumulating so the first frame is delayed by a full
            // 480 samples. The output rate stays equal to the input rate.
            output[i] = self.pop();
            self.input[self.input_pos] = sample;
            self.input_pos += 1;
            if self.input_pos == NOISE_FRAME {
                self.input_pos = 0;
                // RNNoise expects values around i16 full scale.
                for (dst, src) in self.wet.iter_mut().zip(self.input.iter()) {
                    *dst = *src * 32768.0;
                }
                let input_i16 = self.input.map(|v| v * 32768.0);
                self.state.process_frame(&mut self.wet, &input_i16);
                let target = f32::from_bits(self.strength.load(Ordering::Relaxed)).clamp(0.0, 1.0);
                smooth_toward(
                    &mut self.current_strength,
                    target,
                    NOISE_FRAME,
                    GAIN_SMOOTH_SECONDS,
                );
                let strength = self.current_strength;
                let mut mixed = [0.0f32; NOISE_FRAME];
                for (index, (wet, dry)) in self.wet.iter().zip(self.input.iter()).enumerate() {
                    mixed[index] = *wet / 32768.0 * strength + *dry * (1.0 - strength);
                }
                for sample in mixed {
                    self.push(sample);
                }
            }
        }
    }

    fn push(&mut self, sample: f32) {
        if self.queue_len == self.queue.len() {
            // Should not happen at equal input/output rates, but never
            // overwrite unread audio if it does.
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
    fn noise_reducer_has_one_frame_latency_and_same_length() {
        let mut reducer = NoiseReducer::new(1.0, Arc::new(AtomicU32::new(1.0f32.to_bits())));
        let input = vec![0.25; NOISE_FRAME * 3];
        let mut output = vec![0.0; input.len()];
        reducer.process(&input, &mut output);
        assert_eq!(output.len(), input.len());
        assert!(output[..NOISE_FRAME].iter().all(|v| *v == 0.0));
        assert!(output[NOISE_FRAME..].iter().any(|v| *v != 0.0));
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
}
