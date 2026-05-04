use anyhow::Result;
use tokio::time::{sleep, Duration};

use crate::{audio, tts};

pub async fn run() -> Result<()> {
    println!("VoiceForge daemon running. Press Ctrl+C to stop.");

    let engine = tts::select_engine()?;
    loop {
        sleep(Duration::from_secs(30)).await;
        let audio_path = engine
            .speak("VoiceForge daemon is alive.", "tiny_robot")
            .await?;
        audio::play(audio_path.to_str().unwrap_or(""))?;
    }
}
