use anyhow::{Context, Result};
use rodio::{Decoder, OutputStream, Sink};
use std::fs::File;
use std::io::BufReader;

pub fn play(path: &str) -> Result<()> {
    let (_stream, handle) = OutputStream::try_default()
        .context("Could not open default audio output device")?;

    let sink = Sink::try_new(&handle)?;
    let file = File::open(path)
        .with_context(|| format!("Could not open audio file: {}", path))?;
    let source = Decoder::new(BufReader::new(file))?;

    sink.append(source);
    sink.sleep_until_end();

    Ok(())
}
