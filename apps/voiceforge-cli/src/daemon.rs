//! Daemon command entry point. Builds the engine, rules, and audio
//! sink, then hands them to `daemon_server::serve` which owns the
//! event loop. See `daemon_server.rs` for the wire protocol and
//! shutdown / stale-socket / concurrency story.

use anyhow::Result;
use std::sync::Arc;

use crate::audio_sink::{AudioSink, RodioSink};
use crate::cast::Casts;
use crate::daemon_server::{serve, DaemonConfig};
use crate::notify_macos;
use crate::reaction;
use crate::rules::{resolve_rules_path, Rules};
use crate::tts;

pub async fn run() -> Result<()> {
    let socket_path = DaemonConfig::default_path()?;
    let cfg = DaemonConfig { socket_path };

    let engine = Arc::new(tts::select_engine()?);
    let rules = Arc::new(
        resolve_rules_path()
            .and_then(|path| Rules::load(&path).ok())
            .unwrap_or_else(Rules::default_builtin),
    );
    let casts = Arc::new(Casts::load().unwrap_or_else(|e| {
        eprintln!("voiceforge: failed to load casts.toml: {e:#} — continuing with no casts");
        Casts::empty()
    }));
    let provider = reaction::select_provider(Arc::clone(&rules), Arc::clone(&casts));

    // ROADMAP 4.3: warn loudly when casts are configured but the
    // provider is StaticProvider — casts only fire through the LLM.
    if !casts.is_empty() && provider.name() == "static" {
        eprintln!(
            "voiceforge: {} cast(s) configured but no LLM provider — set VOICEFORGE_LLM_URL to enable, or remove casts.toml",
            casts.len()
        );
    }

    let sink: Arc<dyn AudioSink> = Arc::new(RodioSink);
    let mirror = notify_macos::default_mirror();

    serve(cfg, engine, provider, sink, mirror).await
}
