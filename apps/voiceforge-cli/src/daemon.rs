//! Daemon command entry point. Builds the engine, rules, and audio
//! sink, then hands them to `daemon_server::serve` which owns the
//! event loop. See `daemon_server.rs` for the wire protocol and
//! shutdown / stale-socket / concurrency story.

use anyhow::Result;
use std::sync::Arc;

use crate::audio_sink::{AudioSink, RodioSink};
use crate::daemon_server::{serve, DaemonConfig};
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
    let sink: Arc<dyn AudioSink> = Arc::new(RodioSink);

    serve(cfg, engine, rules, sink).await
}
