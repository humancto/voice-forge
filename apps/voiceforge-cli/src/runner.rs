use anyhow::{bail, Result};
use tokio::process::Command;

use crate::config;
use crate::rules::{choose_reaction, resolve_rules_path, Rules};
use crate::{audio, tts};

/// Run an external command and speak a reaction on success / failure.
///
/// `voice_override`:
/// - `Some(name)`: override the rule-selected voice with this one for
///   both success and failure events. Rule-selected text is unchanged
///   (the voice swaps, the line stays the same).
/// - `None`: use the rule's voice if defined, else the user's
///   `active_voice` from `config.toml`, else `default`.
pub async fn run_command(command: Vec<String>, voice_override: Option<String>) -> Result<()> {
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

    // Fallback chain for the (voice, text) pair when rules.json is
    // missing or doesn't cover this event:
    //   text  → rule line, else this hardcoded fallback
    //   voice → --voice override, else rule voice, else active_voice, else "default"
    let active_voice = config::read_active_voice();
    let text_fallback = if status.success() {
        "Command completed successfully."
    } else {
        "Command failed."
    };
    let fallback = (active_voice.as_str(), text_fallback);

    let rules = resolve_rules_path()
        .and_then(|path| Rules::load(&path).ok())
        .unwrap_or_else(Rules::default_builtin);

    let mut rng = rand::thread_rng();
    let (rule_voice, text) = choose_reaction(&rules, event, fallback, &mut rng);

    // --voice flag wins; otherwise the rule's voice; otherwise the
    // user's active voice (already baked in as the fallback above).
    let voice = match voice_override {
        Some(v) => v,
        None => rule_voice,
    };

    let engine = tts::select_engine()?;
    let audio_path = engine.speak(&text, &voice).await?;
    audio::play(audio_path.to_str().unwrap_or(""))?;

    if !status.success() {
        bail!("Command failed with status: {}", status);
    }

    Ok(())
}
