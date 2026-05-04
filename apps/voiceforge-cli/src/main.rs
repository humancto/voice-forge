use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;

mod audio;
mod bootstrap;
mod clone;
mod config;
mod daemon;
mod doctor;
mod ingest;
mod install_cloning;
mod paths;
mod rules;
mod runner;
mod tts;
mod voices;

#[derive(Parser)]
#[command(name = "voiceforge")]
#[command(about = "Local terminal voice runtime", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Say {
        #[arg(long)]
        text: String,
        /// Voice name. When omitted, uses `active_voice` from
        /// ~/.voiceforge/config.toml (set via `voiceforge use`).
        #[arg(long)]
        voice: Option<String>,
    },
    Run {
        /// Voice override. When omitted, uses `active_voice` from
        /// ~/.voiceforge/config.toml. The voice replaces the rule's
        /// voice but does NOT change the rule-selected text.
        #[arg(long)]
        voice: Option<String>,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Daemon,
    /// List voices (built-in presets + cloned). Active voice marked with `*`.
    Voices {
        #[command(subcommand)]
        action: Option<VoicesAction>,
    },
    /// Set the active voice for `voiceforge say` and `voiceforge run`
    /// when no `--voice` is passed. Writes `active_voice` to
    /// ~/.voiceforge/config.toml.
    Use {
        /// Voice name. Must be a built-in preset (see `voiceforge voices`)
        /// or a cloned voice (created via `voiceforge clone`).
        name: String,
    },
    /// System health check — verifies the binary, ~/.voiceforge layout,
    /// audio backend, embedded TTS, optional Python server, cache,
    /// presets, and ffmpeg.
    Doctor {
        /// Output as JSON for tooling. Schema is versioned via
        /// `schema_version` and currently at 1.
        #[arg(long)]
        json: bool,
    },
    /// Transcode any audio source into a canonical XTTS-ready WAV
    /// (22050 Hz mono 16-bit PCM, 10–60 s).
    Ingest {
        /// Path to the source audio (wav/mp3/m4a/ogg/flac/aiff/webm/...).
        input: PathBuf,
        /// Path to the output WAV. Parent dirs are created if missing.
        output: PathBuf,
    },
    /// Clone a voice from a local file. Saves a voice profile under
    /// `~/.voiceforge/voices/<name>/` that `voiceforge say --voice <name>` uses.
    /// Source must be ≥60s of clean single-speaker audio.
    Clone {
        /// Voice name; matches [a-z0-9_-], 1..=32 chars. Cannot be a reserved
        /// name (presets, cache, cloning, voices, embeddings, logs).
        name: String,
        /// Local file path: `/abs/path.wav`, `~/relative.mp3`, or
        /// `file://...`. URLs are not supported — download with your
        /// tool of choice and point at the local file.
        source: String,
        /// Replace an existing voice with the same name.
        #[arg(long)]
        force: bool,
    },
    /// Install the GPT-SoVITS v2 cloning stack into ~/.voiceforge/cloning/.
    /// Idempotent. macOS arm64 only for now (Linux/Windows: ROADMAP 2.1.1).
    InstallCloning {
        /// Wipe venv + marker before installing (preserves HF model cache).
        #[arg(long, conflicts_with_all = ["check", "uninstall"])]
        force: bool,
        /// Verify install state without mutating anything.
        #[arg(long, conflicts_with_all = ["force", "uninstall"])]
        check: bool,
        /// Remove venv + repo + marker (preserves HF model cache).
        #[arg(long, conflicts_with_all = ["force", "check"])]
        uninstall: bool,
    },
}

#[derive(Subcommand)]
enum VoicesAction {
    /// Remove a cloned voice. Built-in presets cannot be removed via
    /// this subcommand — edit `~/.voiceforge/presets/<name>.json`
    /// directly to customize, or delete the file there to revert to the
    /// embedded default on next bootstrap.
    Remove {
        name: String,
        /// Skip the confirmation prompt (required when stdin is not a TTY).
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let bootstrap_report = bootstrap::ensure_voiceforge_home()?;
    bootstrap::print_if_first_run(&bootstrap_report);

    match cli.command {
        Commands::Say { text, voice } => {
            let voice = resolve_voice(voice);
            let engine = tts::select_engine()?;
            let audio_path = engine.speak(&text, &voice).await?;
            audio::play(audio_path.to_str().unwrap_or(""))?;
        }
        Commands::Run { voice, command } => {
            runner::run_command(command, voice).await?;
        }
        Commands::Daemon => {
            daemon::run().await?;
        }
        Commands::Voices { action } => match action {
            None => list_voices_cmd()?,
            Some(VoicesAction::Remove { name, force }) => {
                remove_voice_cmd(&name, force)?;
            }
        },
        Commands::Use { name } => use_voice_cmd(&name)?,
        Commands::Doctor { json } => {
            let report = doctor::run_doctor().await;
            let mut out = std::io::stdout().lock();
            if json {
                doctor::render_json(&report, &mut out)?;
            } else {
                doctor::render_human(&report, &mut out)?;
            }
            if report.has_error() {
                std::process::exit(1);
            }
        }
        Commands::Ingest { input, output } => {
            let report = ingest::ingest(&input, &output, &ingest::IngestConfig::default())?;
            println!(
                "ingested {} -> {} ({} Hz, {} ch, {} bit, {}, {:.2}s)",
                input.display(),
                output.display(),
                report.sample_rate,
                report.channels,
                report.bits_per_sample,
                report.codec,
                report.duration_seconds,
            );
        }
        Commands::InstallCloning {
            force,
            check,
            uninstall,
        } => {
            install_cloning::run(force, check, uninstall)?;
        }
        Commands::Clone {
            name,
            source,
            force,
        } => {
            clone::run(name, source, force)?;
        }
    }

    Ok(())
}

/// Resolve `--voice` flag → user-set active voice → DEFAULT_VOICE.
/// Logs the active-voice fallback once to stderr so users discovering
/// "why does it sound different" see what's happening.
fn resolve_voice(flag: Option<String>) -> String {
    if let Some(v) = flag {
        return v;
    }
    let active = config::read_active_voice();
    if active != config::DEFAULT_VOICE {
        eprintln!("voiceforge: using active voice: {active} (set via `voiceforge use`)");
    }
    active
}

fn use_voice_cmd(name: &str) -> Result<()> {
    voices::validate_name(name).or_else(|e| {
        // Emit a friendlier hint than the bare validate_name error.
        bail!("invalid voice name {name:?}: {e:#}")
    })?;

    // Existence check: built-in preset or cloned voice
    let preset_match = config::load_presets()?.iter().any(|p| p.id == name);
    let cloned_match = voices::voice_exists(name);
    if !preset_match && !cloned_match {
        bail!(
            "voice {name:?} not found. Run `voiceforge voices` to list available voices, or `voiceforge clone {name} <source>` to create one."
        );
    }

    let current = config::read_active_voice();
    if current == name {
        println!("active voice already set to {name:?} — no change.");
        return Ok(());
    }

    config::write_active_voice(name)?;
    println!("active voice set to {name:?}");
    if cloned_match && !preset_match {
        println!("(cloned voice — uses GPT-SoVITS v2 via `voiceforge install-cloning`)");
    }
    Ok(())
}

fn list_voices_cmd() -> Result<()> {
    let active = config::read_active_voice();
    let presets = config::load_presets()?;
    let cloned = voices::list_cloned_voices()?;

    let mut out = std::io::stdout().lock();
    writeln!(out, "BUILT-IN")?;
    for p in &presets {
        let marker = if p.id == active { "*" } else { " " };
        let display = p.display_name.clone().unwrap_or_else(|| p.id.clone());
        writeln!(out, " {marker} {:<24}  {display}", p.id)?;
    }

    if !cloned.is_empty() {
        writeln!(out)?;
        writeln!(out, "CLONED")?;
        for v in &cloned {
            let marker = if v.name == active { "*" } else { " " };
            writeln!(out, " {marker} {:<24}  source: {}", v.name, v.source)?;
        }
    }

    writeln!(out)?;
    writeln!(out, "active: {active}")?;
    Ok(())
}

fn remove_voice_cmd(name: &str, force: bool) -> Result<()> {
    voices::validate_name(name)?;

    // Block removing built-in presets via this command. They're a
    // separate concept (~/.voiceforge/presets/<name>.json restored by
    // bootstrap); deleting here would silently come back next launch.
    let preset_match = config::load_presets()?.iter().any(|p| p.id == name);
    if preset_match {
        bail!(
            "{name:?} is a built-in preset and cannot be removed via this command.\n\
             To customize, edit ~/.voiceforge/presets/{name}.json directly.\n\
             To revert any local edits, delete that file — bootstrap restores the embedded default on next launch."
        );
    }

    if !voices::voice_exists(name) {
        bail!("voice {name:?} not found");
    }

    // Confirm unless --force or stdin is a TTY-less pipeline. In a
    // TTY, prompt; in a non-TTY (CI, piped) require --force.
    let stdin_is_tty = std::io::stdin().is_terminal();
    if !force {
        if !stdin_is_tty {
            bail!(
                "remove {name:?}: stdin is not a TTY and --force was not passed.\n\
                 Add `--force` to remove non-interactively."
            );
        }
        eprint!("remove voice {name:?}? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("aborted.");
            return Ok(());
        }
    }

    voices::remove_cloned_voice(name)?;
    println!("removed voice {name:?}");

    // If the removed voice was the active one, nudge the user to pick
    // a new active voice. We don't auto-rewrite config.toml because
    // that's a state change without explicit consent.
    let active = config::read_active_voice();
    if active == name {
        eprintln!(
            "warning: active voice was {name:?}; voiceforge will fall back to {DEFAULT}.\n\
             Run `voiceforge use <other>` to set a new active voice.",
            DEFAULT = config::DEFAULT_VOICE
        );
    }
    Ok(())
}
