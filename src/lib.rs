//! Browser AudioWorklet adapter for Shredini's shared Rust engine.
#[cfg(test)]
mod allocation_probe;
pub use shredini_audio_core::{dsp, effects, media, midi, model};
pub mod web_engine;
