//! Native PipeWire backend for MVX.
//!
//! M0 scope: microphone capture, native virtual source, playback bridge, gain/mute.

mod engine;

pub use engine::{AudioCommand, AudioEngine, CommandSender};
