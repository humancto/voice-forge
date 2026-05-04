use anyhow::{bail, Result};
use tokio::process::Command;

use crate::rules::{choose_reaction, resolve_rules_path, Rules};
use crate::{audio, tts};

pub async fn run_command(command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        bail!("No command provided");
    }

    let program = &command[0];
    let args = &command[1..];

    println!("Running: {} {}", program, args.join(" "));

    let status = Command::new(program).args(args).status().await?;

    let event = if status.success() {
        "build_success"
    } else {
        "build_failed"
    };

    let fallback = if status.success() {
        ("hype_narrator", "Command completed successfully.")
    } else {
        ("angry_duck", "Command failed.")
    };

    let rules = resolve_rules_path()
        .and_then(|path| Rules::load(&path).ok())
        .unwrap_or_else(Rules::default_builtin);

    let mut rng = rand::thread_rng();
    let (voice, text) = choose_reaction(&rules, event, fallback, &mut rng);

    let engine = tts::select_engine()?;
    let audio_path = engine.speak(&text, &voice).await?;
    audio::play(audio_path.to_str().unwrap_or(""))?;

    if !status.success() {
        bail!("Command failed with status: {}", status);
    }

    Ok(())
}
