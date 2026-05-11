//! Trait-shaped audio playback. Lets the daemon take a sink as a
//! constructor argument so tests can inject a recording sink that
//! doesn't open the audio device.
//!
//! `audio::play` (the free function) stays as the surface for `say`
//! and `run` — those callers don't care about injection and shouldn't
//! pay for the trait indirection. They go through `RodioSink::default()`
//! under the hood, so the playback behavior is identical.

use anyhow::Result;
use std::path::Path;

pub trait AudioSink: Send + Sync {
    /// Blocks until playback finishes. Implementations MUST NOT
    /// return early. The PlaybackQueue (ROADMAP 4.3) relies on this
    /// contract to serialize cast turns — if `play` returned before
    /// the audio actually finished, turn-2 would start while turn-1
    /// was still audible.
    fn play(&self, wav_path: &Path) -> Result<()>;
}

#[derive(Default)]
pub struct RodioSink;

impl AudioSink for RodioSink {
    fn play(&self, wav_path: &Path) -> Result<()> {
        crate::audio::play(wav_path)
    }
}
