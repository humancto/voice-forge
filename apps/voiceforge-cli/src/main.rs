use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod audio;
mod bootstrap;
mod config;
mod daemon;
mod doctor;
mod ingest;
mod install_cloning;
mod paths;
mod rules;
mod runner;
mod tts;

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
        #[arg(long, default_value = "default")]
        voice: String,
    },
    Run {
        #[arg(last = true)]
        command: Vec<String>,
    },
    Daemon,
    Voices,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Eager: every subcommand benefits from a populated ~/.voiceforge/.
    // Lazy bootstrap per-handler is a bug-farm.
    let bootstrap_report = bootstrap::ensure_voiceforge_home()?;
    bootstrap::print_if_first_run(&bootstrap_report);

    match cli.command {
        Commands::Say { text, voice } => {
            let engine = tts::select_engine()?;
            let audio_path = engine.speak(&text, &voice).await?;
            audio::play(audio_path.to_str().unwrap_or(""))?;
        }
        Commands::Run { command } => {
            runner::run_command(command).await?;
        }
        Commands::Daemon => {
            daemon::run().await?;
        }
        Commands::Voices => {
            let voices = config::load_presets()?;
            for voice in voices {
                println!(
                    "{} - {}",
                    voice.id,
                    voice.display_name.unwrap_or_else(|| voice.id.clone())
                );
            }
        }
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
    }

    Ok(())
}
