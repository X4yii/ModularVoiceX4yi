//! Native PipeWire backend for MVX.
//!
//! Microphone capture, the voice-processing chain, and the native virtual source.

#![allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::needless_range_loop
)]

mod dsp;
mod engine;
pub mod neural_vc;
mod speaker;

pub use engine::{AudioCommand, AudioEngine, CommandSender};
