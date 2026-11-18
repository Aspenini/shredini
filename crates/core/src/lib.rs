//! Audio processing, media decoding and instruments shared by desktop and web.
#[cfg(test)]
mod allocation_probe;
pub mod dsp;
pub mod effects;
pub mod media;
pub mod midi;
pub mod model;
#[cfg(not(target_arch = "wasm32"))]
pub mod wav;
