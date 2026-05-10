use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;

mod audio;
mod audio_sink;
mod bootstrap;
mod clone;
mod config;
mod daemon;
mod daemon_client;
mod daemon_server;
mod doctor;
mod hook;
mod ingest;
mod install_cloning;
mod packs;
mod paths;
mod rules;
mod runner;
mod sentinel;
mod shell_init;
mod tts;
mod url_ingest;
mod voices;

#[derive(Parser)]
#[command(name = "voiceforge", version)]
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
    /// Read NDJSON events from stdin, forward each to the daemon.
    /// Designed for AI-agent integrations (Claude Code, Cursor, etc.)
    /// that emit one JSON object per line. Use `--profile claude-code`
    /// for the Claude Code hook payload schema. Distinct from
    /// `voiceforge ingest` (audio) and `voiceforge send` (single-frame).
    Hook {
        /// Pull the event name from a dotted JSON path
        /// (e.g. `hook.event_name`).
        #[arg(long)]
        event_from: Option<String>,
        /// Pull the message from a dotted JSON path. Default: forward
        /// the whole line (capped at 4 KiB).
        #[arg(long)]
        message_from: Option<String>,
        /// Override per-frame voice for the whole stream.
        #[arg(long)]
        voice: Option<String>,
        /// Use a known upstream's field layout. Supported: `claude-code`.
        /// Mutually exclusive with --event-from / --message-from.
        #[arg(long, conflicts_with_all = ["event_from", "message_from"])]
        profile: Option<String>,
        /// Re-emit each input line on stdout BEFORE forwarding (so a
        /// downstream `jq` or `tee` doesn't block on a hung daemon).
        #[arg(long)]
        passthrough: bool,
        /// Suppress per-frame stderr warnings.
        #[arg(long)]
        quiet: bool,
    },
    /// Print or install zsh / bash hook scripts that fire
    /// command_succeeded / command_failed daemon events for commands
    /// over a configurable threshold (default 3 s, env override
    /// VOICEFORGE_SHELL_THRESHOLD_MS). Companion to `voiceforge daemon`
    /// + `voiceforge send`.
    ShellInit {
        /// Shell to render. `zsh` or `bash`. Default behavior prints
        /// the hook to stdout for `eval "$(voiceforge shell-init zsh)"`.
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(["zsh", "bash"]))]
        shell: Option<String>,
        /// Append the hook block to ~/.zshrc or ~/.bashrc (idempotent).
        #[arg(long, conflicts_with_all = ["uninstall", "status"])]
        install: bool,
        /// Strip the hook block from ~/.zshrc or ~/.bashrc.
        #[arg(long, conflicts_with_all = ["install", "status"])]
        uninstall: bool,
        /// Report which shells have the hook installed.
        #[arg(long, conflicts_with_all = ["install", "uninstall"])]
        status: bool,
        /// Override the stale-invocation guard during install.
        #[arg(long)]
        force: bool,
    },
    /// Drop one event onto the daemon's Unix socket and print its
    /// reply. Companion to `voiceforge daemon`. Either `event` or
    /// `--text` must be supplied. Exit codes: 0 on `ok:true`, 1 on
    /// `ok:false`, 2 on not-reachable, 4 on post-connect protocol
    /// failure.
    Send {
        /// Event name (looked up in the rules table). Required unless
        /// `--text` is supplied.
        event: Option<String>,
        /// Bypass the rules table — speak this verbatim. Wins over
        /// the rule's text.
        #[arg(long)]
        text: Option<String>,
        /// Override the rule's voice. When `--text` is set without
        /// `--voice`, defaults to "default".
        #[arg(long)]
        voice: Option<String>,
        /// Free-form context (logged by daemon, not spoken). Forward-
        /// compat with claude-code hook payloads.
        #[arg(long)]
        message: Option<String>,
        /// Print the raw daemon reply line on stdout instead of a
        /// human summary.
        #[arg(long)]
        json: bool,
    },
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
    /// Transcode any audio source into a canonical GPT-SoVITS-ready WAV
    /// (32000 Hz mono 16-bit PCM, 10–60 s).
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
    /// Play a pre-rendered WAV from an installed pack
    /// (`~/.voiceforge/packs/<pack>/wav/<event>.wav`). Sub-100ms warm
    /// path — no TTS engine, no model load, just file lookup + playback.
    ///
    /// Exit codes:
    ///   0 — played successfully
    ///   2 — pack not installed (or invalid pack/event name)
    ///   3 — event not in pack (caller may fall back via `voiceforge say`)
    ///   4 — WAV present but undecodable
    ///   5 — audio backend unavailable
    Play {
        /// Pack to look up. Must be installed under
        /// `~/.voiceforge/packs/<NAME>/`. See `voiceforge pack install`
        /// to fetch + install one.
        #[arg(long, required_unless_present = "list")]
        pack: Option<String>,
        /// Event id; resolves to `<pack>/wav/<event>.wav`.
        #[arg(long, required_unless_present = "list")]
        event: Option<String>,
        /// Print sorted event ids in the pack and exit.
        #[arg(long, requires = "pack")]
        list: bool,
    },
    /// Manage installed voice packs. Pulls from a static pack index
    /// (default voice-forge-packs repo); override via
    /// `VOICEFORGE_PACK_INDEX_URL`.
    ///
    /// Exit codes (per subaction): 0 success, 2 unknown pack, 3 fetch
    /// failed, 4 sha256 mismatch, 5 extract or disk failure, 6 already
    /// installed (without --force) or concurrent install in progress.
    Pack {
        #[command(subcommand)]
        action: PackAction,
    },
}

#[derive(Subcommand)]
enum PackAction {
    /// List packs available in the index, with installed status.
    List {
        /// Output as JSON for tooling.
        #[arg(long)]
        json: bool,
    },
    /// Download + install a pack.
    Install {
        /// Pack name (must appear in the index).
        name: String,
        /// Replace any existing install.
        #[arg(long)]
        force: bool,
    },
    /// Remove an installed pack.
    Remove {
        name: String,
        /// Skip the confirmation prompt (required when stdin is not a TTY).
        #[arg(long)]
        force: bool,
    },
    /// Show pack metadata (manifest.toml). Reads from disk if installed,
    /// else fetches the manifest URL from the index.
    Info { name: String },
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
            // Pack-first dispatch: if `voice` names an installed pack,
            // try to resolve `text` to one of its 13 pre-rendered
            // events (by event_id, exact phrase text, or fuzzy
            // contains). Hit -> sub-100ms WAV playback, no synth, no
            // model load. Miss -> clear error pointing at the pack's
            // event list. The user installed a 9MB pack and shouldn't
            // need to also install the 1.7GB cloning runtime just to
            // hear it.
            //
            // Falls through to the synth engine ONLY when the voice is
            // NOT an installed pack (built-in preset / cloned voice /
            // unknown -> existing engine.speak path which has its own
            // routing).
            if packs::pack_is_installed(&voice) {
                match packs::resolve_text_to_event(&voice, &text) {
                    Ok(Some(event)) => {
                        let wav = packs::resolve_event_wav(&voice, &event)?;
                        eprintln!(
                            "voiceforge: pack {voice:?} matched event {event:?} -- playing pre-rendered WAV"
                        );
                        audio::play(wav.to_str().unwrap_or(""))?;
                        return Ok(());
                    }
                    Ok(None) => {
                        let events = packs::list_events(&voice).unwrap_or_default();
                        let preview = events
                            .iter()
                            .take(6)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        let more = if events.len() > 6 {
                            format!(" (+{} more)", events.len() - 6)
                        } else {
                            String::new()
                        };
                        bail!(
                            "voice {voice:?} is installed as a PACK with {} pre-rendered phrases.\n\
                             your text didn't match any phrase. available events: {preview}{more}.\n\
                             \n\
                             - see all phrases:   voiceforge play --pack {voice} --list\n\
                             - play one:          voiceforge play --pack {voice} --event <id>\n\
                             - say verbatim:      use one of the pack's phrase strings\n\
                             - arbitrary text:    install cloning then `voiceforge clone {voice} <60s.wav>`",
                            manifest_phrases_count(&voice),
                        );
                    }
                    Err(e) => {
                        bail!("voice {voice:?} is installed as a pack but its manifest could not be read: {e:#}");
                    }
                }
            }
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
        Commands::Send {
            event,
            text,
            voice,
            message,
            json,
        } => {
            let exit_code = run_send(event, text, voice, message, json).await;
            std::process::exit(exit_code);
        }
        Commands::ShellInit {
            shell,
            install,
            uninstall,
            status,
            force,
        } => {
            let exit_code = run_shell_init(shell, install, uninstall, status, force);
            std::process::exit(exit_code);
        }
        Commands::Hook {
            event_from,
            message_from,
            voice,
            profile,
            passthrough,
            quiet,
        } => {
            let exit_code =
                run_hook(event_from, message_from, voice, profile, passthrough, quiet).await;
            std::process::exit(exit_code);
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
            // Route URLs through url_ingest::resolve_source first so
            // the existing path-typed ingest::ingest stays pure. The
            // ResolvedSource value lives until the end of this arm; if
            // it carries a tempdir, that tempdir survives the ingest
            // call. ROADMAP 2.3.
            let input_str = input.to_string_lossy().into_owned();
            let resolved = url_ingest::resolve_source(&input_str)
                .with_context(|| format!("resolving ingest input {input_str:?}"))?;
            let local_input = resolved.local_path();
            let report = ingest::ingest(local_input, &output, &ingest::IngestConfig::default())?;
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
            drop(resolved);
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
        Commands::Play { pack, event, list } => {
            // The runtime path for ROADMAP 6.5 — sub-100ms WAV playback
            // from an installed pack. Distinct from `say` so the TTS
            // engine startup cost never enters the hot path.
            play_cmd(pack, event, list).await?;
        }
        Commands::Pack { action } => {
            // ROADMAP 6.2 — pack distribution surface. Each subaction
            // handles its own exit codes via `process::exit` so the
            // documented contract holds even when bubbling errors.
            pack_cmd(action).await?;
        }
    }

    Ok(())
}

/// `voiceforge pack` dispatch. Each subaction calls `process::exit` on
/// failure with the InstallError's exit_code, so the structured contract
/// holds for callers (e.g. `voiceforge hook` shelling out per event).
async fn pack_cmd(action: PackAction) -> Result<()> {
    use packs::{InstallError, PackEntry};

    fn die_install(err: InstallError) -> ! {
        eprintln!("voiceforge pack: {err}");
        std::process::exit(err.exit_code());
    }

    match action {
        PackAction::List { json } => {
            // Fetch index; merge with installed-list to compute status.
            let index = packs::fetch_index()
                .await
                .unwrap_or_else(|e| die_install(e));
            let installed: std::collections::BTreeSet<String> = packs::list_installed()
                .unwrap_or_default()
                .into_iter()
                .collect();

            if json {
                #[derive(serde::Serialize)]
                struct Row<'a> {
                    name: &'a str,
                    installed: bool,
                    entry: &'a PackEntry,
                }
                let rows: Vec<Row> = index
                    .packs
                    .iter()
                    .map(|(name, entry)| Row {
                        name,
                        installed: installed.contains(name),
                        entry,
                    })
                    .collect();
                let mut out = std::io::stdout().lock();
                serde_json::to_writer_pretty(&mut out, &rows)?;
                writeln!(out)?;
            } else {
                let mut out = std::io::stdout().lock();
                writeln!(
                    out,
                    "NAME                 STATUS    VERSION    TIER           DISPLAY NAME"
                )?;
                for (name, entry) in &index.packs {
                    let status = if installed.contains(name) {
                        "installed"
                    } else {
                        "available"
                    };
                    let version = &entry.version;
                    let tier = &entry.tier;
                    let display = &entry.display_name;
                    writeln!(
                        out,
                        "{name:<20} {status:<9} {version:<10} {tier:<14} {display}"
                    )?;
                }
            }
        }
        PackAction::Install { name, force } => {
            // Run the full install flow. Errors map to documented exit codes.
            let entry = packs::install_pack(&name, force)
                .await
                .unwrap_or_else(|e| die_install(e));
            println!(
                "installed {} v{} ({} phrases). Try: voiceforge play --pack {} --event tests_passed",
                name, entry.version, entry.phrases, name
            );
        }
        PackAction::Remove { name, force } => {
            // TTY-detect for confirm prompt; --force required in non-TTY.
            // Mirrors `voiceforge voices remove`.
            let stdin_is_tty = std::io::stdin().is_terminal();
            if !force {
                if !stdin_is_tty {
                    eprintln!(
                        "voiceforge pack remove {name:?}: stdin is not a TTY and --force was not passed."
                    );
                    std::process::exit(2);
                }
                eprint!("remove pack {name:?}? [y/N] ");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if !line.trim().eq_ignore_ascii_case("y") {
                    println!("aborted.");
                    return Ok(());
                }
            }
            packs::remove_pack(&name).unwrap_or_else(|e| die_install(e));
            println!("removed pack {name:?}");
        }
        PackAction::Info { name } => {
            // Try installed-on-disk first; fall through to the index
            // entry if not installed (best-effort, no separate manifest
            // fetch in this PR — index has the same display info).
            match packs::pack_info_local(&name) {
                Ok(m) => {
                    println!("name:               {}", m.name);
                    println!("display_name:       {}", m.display_name);
                    println!("description:        {}", m.description);
                    println!("voice_source:       {}", m.voice_source);
                    println!("source_clip_url:    {}", m.source_clip_url);
                    println!("source_clip_episode: {}", m.source_clip_episode);
                    println!("rendered_with:      {}", m.rendered_with);
                    println!("rendered_at:        {}", m.rendered_at);
                    println!("sample_rate:        {}", m.sample_rate);
                    println!("phrases:            {}", m.phrases);
                    println!("license:            {}", m.license);
                    println!("(installed locally; reading manifest.toml from disk)");
                }
                Err(InstallError::UnknownPack(_)) => {
                    // Not installed — fetch the index for whatever info we have.
                    let index = packs::fetch_index()
                        .await
                        .unwrap_or_else(|e| die_install(e));
                    let entry = index
                        .packs
                        .get(&name)
                        .unwrap_or_else(|| die_install(InstallError::UnknownPack(name.clone())));
                    println!("name:               {}", name);
                    println!("display_name:       {}", entry.display_name);
                    println!("description:        {}", entry.description);
                    println!("voice_source:       {}", entry.voice_source);
                    println!("source_clip_url:    {}", entry.source_clip_url);
                    println!("version:            {}", entry.version);
                    println!("tier:               {}", entry.tier);
                    println!("phrases:            {}", entry.phrases);
                    println!("sample_rate:        {}", entry.sample_rate);
                    println!("license:            {}", entry.license);
                    println!("tarball_url:        {}", entry.tarball_url);
                    println!("tarball_sha256:     {}", entry.tarball_sha256);
                    println!(
                        "(not installed; install with: voiceforge pack install {})",
                        name
                    );
                }
                Err(e) => die_install(e),
            }
        }
    }
    Ok(())
}

/// `voiceforge play` dispatch. Returns to main, which exits 0 on success;
/// errors carry their own exit codes via `PlayError::exit_code()`.
async fn play_cmd(pack: Option<String>, event: Option<String>, list: bool) -> Result<()> {
    // clap's required_unless_present + requires gates ensure these are set
    // when we reach this branch; unwrap is safe.
    let pack = pack.expect("clap guards --pack required unless --list");

    if list {
        match packs::list_events(&pack) {
            Ok(events) => {
                let mut out = std::io::stdout().lock();
                for e in events {
                    writeln!(out, "{e}")?;
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("voiceforge play --list: {e}");
                std::process::exit(e.exit_code());
            }
        }
    } else {
        let event = event.expect("clap guards --event required unless --list");
        let wav = match packs::resolve_event_wav(&pack, &event) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("voiceforge play: {e}");
                std::process::exit(e.exit_code());
            }
        };
        // rodio's sleep_until_end blocks; spawn_blocking keeps the
        // tokio runtime free for other work (none, in this CLI, but
        // future daemon mode will care).
        let result = tokio::task::spawn_blocking(move || packs::play_wav(&wav))
            .await
            .map_err(|e| anyhow::anyhow!("play task join error: {e}"))?;
        if let Err(e) = result {
            eprintln!("voiceforge play: {e}");
            std::process::exit(e.exit_code());
        }
        Ok(())
    }
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

/// Read a duration env override, defaulting if unset / unparseable /
/// out of range. Silently falls back so a malformed env var doesn't
/// brick the CLI.
fn duration_env(key: &str, default_ms: u64, min_ms: u64, max_ms: u64) -> std::time::Duration {
    let raw = std::env::var(key).ok();
    let parsed = raw
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n.clamp(min_ms, max_ms))
        .unwrap_or(default_ms);
    std::time::Duration::from_millis(parsed)
}

/// `voiceforge send` dispatcher. Returns the process exit code.
/// Stays in main.rs (not in `daemon_client`) so env reads / output
/// formatting / exit-code mapping live at the CLI boundary; the
/// `daemon_client::send` API is a pure function of its arguments.
async fn run_send(
    event: Option<String>,
    text: Option<String>,
    voice: Option<String>,
    message: Option<String>,
    json: bool,
) -> i32 {
    if event.is_none() && text.is_none() {
        eprintln!(
            "voiceforge send: must supply either an event name or --text. See `voiceforge send --help`."
        );
        return 3;
    }

    let socket_path = match daemon_server::DaemonConfig::default_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("voiceforge send: {e:#}");
            return 2;
        }
    };

    let connect_timeout = duration_env("VOICEFORGE_SEND_TIMEOUT_MS", 1000, 50, 30_000);
    let read_timeout = duration_env("VOICEFORGE_SEND_READ_TIMEOUT_MS", 5000, 100, 60_000);

    let req = daemon_client::SendRequest {
        event,
        text,
        voice,
        message,
    };

    match daemon_client::send(&socket_path, &req, connect_timeout, read_timeout).await {
        Ok(daemon_client::SendOutcome::Ok { spoken, voice }) => {
            if json {
                // Re-serialize so output is canonical.
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "spoken": spoken, "voice": voice})
                );
            } else {
                println!("spoken: {spoken:?} (voice: {voice})");
            }
            0
        }
        Ok(daemon_client::SendOutcome::Rejected { error }) => {
            if json {
                println!("{}", serde_json::json!({"ok": false, "error": error}));
            } else {
                eprintln!("voiceforge send: daemon rejected frame: {error}");
            }
            1
        }
        Err(daemon_client::SendError::NotReachable(msg)) => {
            eprintln!("voiceforge send: {msg}");
            2
        }
        Err(daemon_client::SendError::Protocol(msg)) => {
            eprintln!("voiceforge send: {msg}");
            4
        }
    }
}

/// `voiceforge hook` dispatcher. Returns the process exit code.
async fn run_hook(
    event_from: Option<String>,
    message_from: Option<String>,
    voice: Option<String>,
    profile: Option<String>,
    passthrough: bool,
    quiet: bool,
) -> i32 {
    if let Some(p) = &profile {
        if let Err(e) = hook::validate_profile(p) {
            eprintln!("voiceforge hook: {e:#}");
            return 3;
        }
    }

    let socket_path = match daemon_server::DaemonConfig::default_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("voiceforge hook: {e:#}");
            return 2;
        }
    };

    let connect_timeout = duration_env("VOICEFORGE_SEND_TIMEOUT_MS", 1000, 50, 30_000);
    let read_timeout = duration_env("VOICEFORGE_SEND_READ_TIMEOUT_MS", 5000, 100, 60_000);
    let fail_window = u64_env("VOICEFORGE_HOOK_FAIL_WINDOW", 100, 1, 10_000) as usize;
    let fail_min = u64_env("VOICEFORGE_HOOK_FAIL_MIN", 10, 1, 10_000) as usize;
    let fail_ratio = f64_env("VOICEFORGE_HOOK_FAIL_RATIO", 0.5, 0.0, 1.0);

    let cfg = hook::HookConfig {
        event_from,
        message_from,
        voice,
        profile,
        passthrough,
        quiet,
        fail_ratio,
        fail_window,
        fail_min,
        connect_timeout,
        read_timeout,
    };

    hook::run(cfg, &socket_path).await
}

fn u64_env(key: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n.clamp(min, max))
        .unwrap_or(default)
}

fn f64_env(key: &str, default: f64, min: f64, max: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|n| n.clamp(min, max))
        .unwrap_or(default)
}

/// `voiceforge shell-init` dispatcher. Returns the process exit code.
fn run_shell_init(
    shell: Option<String>,
    install: bool,
    uninstall: bool,
    status: bool,
    force: bool,
) -> i32 {
    use shell_init::{BinaryHint, Shell};
    use std::str::FromStr;

    if status {
        let home = match std::env::var_os("HOME") {
            Some(h) => std::path::PathBuf::from(h),
            None => {
                eprintln!("voiceforge shell-init: $HOME is unset");
                return 2;
            }
        };
        for s in shell_init::status(&home) {
            let mark = if s.installed {
                "installed"
            } else {
                "missing  "
            };
            println!("{} {} {}", mark, s.shell.name(), s.rc_path.display());
        }
        return 0;
    }

    let shell_name = match shell {
        Some(s) => s,
        None => {
            eprintln!(
                "voiceforge shell-init: must supply <shell> (zsh or bash). See `voiceforge shell-init --help`."
            );
            return 3;
        }
    };
    let shell = match Shell::from_str(&shell_name) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("voiceforge shell-init: {e}");
            return 3;
        }
    };

    let hint = BinaryHint::DiscoverViaPath;

    if install {
        let home = match std::env::var_os("HOME") {
            Some(h) => std::path::PathBuf::from(h),
            None => {
                eprintln!("voiceforge shell-init: $HOME is unset");
                return 2;
            }
        };
        let rc_path = home.join(shell.rc_filename());
        match shell_init::install(shell, &rc_path, &hint, force) {
            Ok(report) => {
                let verb = match report.action {
                    shell_init::InstallAction::Created => "appended hook to",
                    shell_init::InstallAction::Replaced => "replaced hook in",
                };
                println!("voiceforge: {} {}", verb, report.rc_path.display());
                println!(
                    "hint: open a new shell or `source {}`",
                    report.rc_path.display()
                );
                0
            }
            Err(e) => {
                eprintln!("voiceforge shell-init: {e:#}");
                1
            }
        }
    } else if uninstall {
        let home = match std::env::var_os("HOME") {
            Some(h) => std::path::PathBuf::from(h),
            None => {
                eprintln!("voiceforge shell-init: $HOME is unset");
                return 2;
            }
        };
        let rc_path = home.join(shell.rc_filename());
        match shell_init::uninstall(&rc_path) {
            Ok(report) => {
                let verb = match report.action {
                    shell_init::UninstallAction::Removed => "removed hook from",
                    shell_init::UninstallAction::NotPresent => "no hook block in",
                };
                println!("voiceforge: {} {}", verb, report.rc_path.display());
                0
            }
            Err(e) => {
                eprintln!("voiceforge shell-init: {e:#}");
                1
            }
        }
    } else {
        // Default: print hook to stdout for `eval "$(voiceforge shell-init zsh)"`.
        print!("{}", shell_init::render_hook(shell, &hint));
        0
    }
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
    let installed_packs: Vec<String> = packs::list_installed().unwrap_or_default();

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

    if !installed_packs.is_empty() {
        writeln!(out)?;
        writeln!(
            out,
            "PACKS  (pre-rendered fixed phrases — use 'voiceforge play --pack <name>'"
        )?;
        writeln!(
            out,
            "        or 'voiceforge say --voice <name> --text <event-or-phrase>')"
        )?;
        for name in &installed_packs {
            let info = packs::pack_info_local(name).ok();
            let display = info
                .as_ref()
                .map(|m| m.display_name.clone())
                .unwrap_or_else(|| name.clone());
            let count = info.as_ref().map(|m| m.phrases).unwrap_or(0);
            let tier = info
                .as_ref()
                .map(|m| m.voice_source.clone())
                .unwrap_or_default();
            let marker = if name == &active { "*" } else { " " };
            let suffix = if tier.is_empty() {
                format!("{count} phrases")
            } else {
                format!("{count} phrases — {tier}")
            };
            writeln!(out, " {marker} {:<24}  {display} ({suffix})", name)?;
        }
    }

    writeln!(out)?;
    writeln!(out, "active: {active}")?;
    Ok(())
}

/// Read the phrase count from a pack's manifest. Used by the pack-
/// miss error message in `Commands::Say`. Returns 0 on any failure;
/// the message is informational, not a correctness gate.
fn manifest_phrases_count(pack: &str) -> u32 {
    packs::pack_info_local(pack).map(|m| m.phrases).unwrap_or(0)
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
