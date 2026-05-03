use anyhow::Result;
use clap::{Parser, Subcommand};

mod audio;
mod cache;
mod config;
mod daemon;
mod runner;
mod tts_client;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Say { text, voice } => {
            let audio_path = tts_client::speak(&text, &voice).await?;
            audio::play(&audio_path)?;
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
                println!("{} - {}", voice.id, voice.display_name.unwrap_or_else(|| voice.id.clone()));
            }
        }
    }

    Ok(())
}
