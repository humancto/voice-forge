use anyhow::{bail, Result};
use std::process::Command;

use crate::{audio, tts_client};

pub async fn run_command(command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        bail!("No command provided");
    }

    let program = &command[0];
    let args = &command[1..];

    println!("Running: {} {}", program, args.join(" "));

    let status = Command::new(program).args(args).status()?;

    let (voice, text) = if status.success() {
        ("hype_narrator", "Command completed successfully.")
    } else {
        ("angry_duck", "Command failed.")
    };

    let audio_path = tts_client::speak(text, voice).await?;
    audio::play(&audio_path)?;

    if !status.success() {
        bail!("Command failed with status: {}", status);
    }

    Ok(())
}
