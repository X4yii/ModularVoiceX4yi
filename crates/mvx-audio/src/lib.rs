//! Native PipeWire backend for MVX.
//!
//! Microphone capture, the voice-processing chain, and the native virtual source.

mod dsp;
mod engine;
mod speaker;

pub use engine::{AudioCommand, AudioEngine, CommandSender};
