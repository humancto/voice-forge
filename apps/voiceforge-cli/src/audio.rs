use anyhow::{Context, Result};
use rodio::{Decoder, OutputStream, Sink};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Block until the WAV finishes playing on the default output.
///
/// Takes `&Path` (not `&str`) so non-UTF-8 paths surface a real
/// `File::open` error instead of being silently coerced to "" by
/// `unwrap_or("")` and producing a confusing "Could not open audio
/// file: " (empty string) error. Caller doesn't need to round-trip
/// PathBuf through to_str.
pub fn play(path: &Path) -> Result<()> {
    let (_stream, handle) =
        OutputStream::try_default().context("Could not open default audio output device")?;

    let sink = Sink::try_new(&handle)?;
    let file = File::open(path)
        .with_context(|| format!("Could not open audio file: {}", path.display()))?;
    let source = Decoder::new(BufReader::new(file))?;

    sink.append(source);
    sink.sleep_until_end();

    Ok(())
}
