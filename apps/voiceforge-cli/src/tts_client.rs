use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct TtsRequest<'a> {
    text: &'a str,
    voice: &'a str,
}

#[derive(Debug, Deserialize)]
struct TtsResponse {
    audio_path: String,
    cache_hit: bool,
}

pub async fn speak(text: &str, voice: &str) -> Result<String> {
    let client = Client::new();

    let base =
        std::env::var("VOICEFORGE_TTS_URL").unwrap_or_else(|_| "http://127.0.0.1:5555".to_string());
    let url = format!("{}/tts", base.trim_end_matches('/'));

    let response = client
        .post(&url)
        .json(&TtsRequest { text, voice })
        .send()
        .await
        .context(
            "Could not connect to local TTS server. Start services/tts-server/server.py first.",
        )?;

    let status = response.status();

    if !status.is_success() {
        anyhow::bail!("TTS server returned error status: {}", status);
    }

    let body: TtsResponse = response.json().await?;
    if body.cache_hit {
        println!("Using cached audio");
    }

    Ok(body.audio_path)
}
