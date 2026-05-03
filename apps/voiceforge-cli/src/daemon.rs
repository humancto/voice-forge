use anyhow::Result;
use tokio::time::{sleep, Duration};

use crate::{audio, tts_client};

pub async fn run() -> Result<()> {
    println!("VoiceForge daemon running. Press Ctrl+C to stop.");

    loop {
        sleep(Duration::from_secs(30)).await;
        let audio_path = tts_client::speak("VoiceForge daemon is alive.", "tiny_robot").await?;
        audio::play(&audio_path)?;
    }
}
