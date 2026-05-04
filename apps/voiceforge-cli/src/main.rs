use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod audio;
mod config;
mod daemon;
mod ingest;
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
    /// Transcode any audio source into a canonical XTTS-ready WAV
    /// (22050 Hz mono 16-bit PCM, 10–60 s).
    Ingest {
        /// Path to the source audio (wav/mp3/m4a/ogg/flac/aiff/webm/...).
        input: PathBuf,
        /// Path to the output WAV. Parent dirs are created if missing.
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

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
    }

    Ok(())
}
