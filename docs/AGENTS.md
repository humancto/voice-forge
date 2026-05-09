# AI Agent Integration

VoiceForge is built to give AI coding agents a voice. This guide
shows the four moving pieces and exactly how to wire each major
agent (Claude Code, Cursor, Codex, generic) into them.

> **Prerequisite for everything below**: install voiceforge once, run
> the daemon. Both are 5-second, no-toolchain operations.
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash
> voiceforge daemon &       # background; speaks events forwarded to it
> disown                    # detach so the daemon survives shell exit
> ```

---

## The four surfaces

| Surface                    | What it is                                                           | When the agent uses it                                    |
| -------------------------- | -------------------------------------------------------------------- | --------------------------------------------------------- |
| `voiceforge daemon`        | Long-running Unix-socket server at `~/.voiceforge/voiceforge.sock`.  | Always running in the background. The single recipient.   |
| `voiceforge send <event>`  | One-shot CLI client. Opens socket, writes one frame, exits.          | Agent runs a shell command per event ("on tool use").    |
| `voiceforge hook`          | Streaming NDJSON forwarder. Reads one JSON object per stdin line.    | Agent emits structured events as a stream (Claude Code).  |
| `voiceforge shell-init`    | zsh / bash hook — auto-fires `command_succeeded` / `command_failed`. | Catches everything the agent runs in your shell.          |

All four exist on `main` today (ROADMAP 1.8 / 1.9 / 3.3 / 3.1).

---

## Claude Code

Claude Code emits a JSON payload per hook event. `voiceforge hook
--profile claude-code` knows the payload shape and maps
`hook_event_name` → daemon `event` automatically.

**Setup** — add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Notification": [
      { "command": "voiceforge hook --profile claude-code" }
    ],
    "Stop": [
      { "command": "voiceforge hook --profile claude-code" }
    ],
    "PreToolUse": [
      { "command": "voiceforge hook --profile claude-code" }
    ]
  }
}
```

Then start (or restart) Claude Code. Every Notification / Stop /
PreToolUse event becomes a daemon frame. The daemon picks a line
from the rules table and speaks it in the active voice (or pack).

**Customizing per event** — edit `~/.voiceforge/rules/events.json`
(or `configs/rules/events.json` in the repo) to add reactions for
the Claude Code event names:

```json
{
  "Notification": {
    "voice": "peter",
    "lines": ["heads up", "your attention please", "look here"]
  },
  "Stop": {
    "voice": "trump",
    "lines": ["agent finished, tremendous work", "all done"]
  }
}
```

**Why `--profile claude-code`?** Without it, you'd write
`--event-from hook_event_name --message-from message`. The profile
encodes that mapping.

---

## Cursor / Continue / Aider

These tend to expose either a "run shell command on event" hook or
an extension API. The shell-command path uses `voiceforge send`
directly.

**Cursor** — in your Cursor task or extension command:

```bash
voiceforge send agent_done --message "completed code review"
voiceforge send command_failed --message "tests broke after refactor"
```

Exit codes: `0` daemon spoke OK; `1` daemon rejected the frame; `2`
daemon not reachable. The daemon picks a reaction line based on
`event`; `--message` is logged but not spoken (it's there for
context in the daemon log).

**Continue / Aider / etc.** — any plugin that can run a shell
command on tool-use can call `voiceforge send`.

---

## Generic streaming source (custom agents, MCP servers, log files)

If your agent emits one JSON object per line on a stream (stdout,
log file, NDJSON HTTP body, MCP server output), pipe it through
`voiceforge hook`:

```bash
# Tail a logfile
tail -f ~/.my-agent/events.log | voiceforge hook --event-from event_type

# Pipe stdout from a long-running agent
my-agent --stream | voiceforge hook --event-from kind --voice peter

# With passthrough — downstream consumer also sees the lines
my-agent --stream | voiceforge hook --event-from kind --passthrough | jq
```

Field extraction supports dotted paths (objects only):

```bash
echo '{"hook":{"event_name":"build_failed"}}' \
  | voiceforge hook --event-from hook.event_name
```

---

## Catch-all: shell-init for terminal-based agents

If your agent spawns shell commands directly (most do at some
point), `voiceforge shell-init` fires `command_succeeded` or
`command_failed` for any command over a threshold (default 3 s,
env-tunable via `VOICEFORGE_SHELL_THRESHOLD_MS`).

```bash
voiceforge shell-init --install zsh    # idempotent; one-time
# Restart shell or `source ~/.zshrc`
```

This catches commands the agent runs that you don't have a
specific hook for — `npm test`, `cargo build`, `pytest`, `pulumi
up`, etc. Skip-list defaults skip `cd / ls / pwd / clear / history /
voiceforge` so prompt-noise stays silent. Configurable via
`VOICEFORGE_SHELL_SKIP=...:colon:list`.

---

## Voice selection per event

Three ways to control which voice speaks:

1. **Per-call override**: `voiceforge send foo --voice peter` or
   `voiceforge hook --voice peter` (stream-wide override).
2. **Active voice**: `voiceforge use peter` writes
   `~/.voiceforge/config.toml`; everything that doesn't override
   uses this.
3. **Per-event in rules**: edit `events.json` so `command_failed`
   uses `angry_duck` and `agent_done` uses `peter`.

Precedence: per-call > rule > active voice > "default".

---

## Latency: which surface to use when

| Source                            | Latency       | Notes                                                                                |
| --------------------------------- | ------------- | ------------------------------------------------------------------------------------ |
| `voiceforge play --pack X --event Y` | **~50 ms**    | Just plays a WAV. Use for events you have pre-rendered.                              |
| `voiceforge say --voice <pack>`   | ~50 ms (pack) | Falls back to live cloning if no matching pack event.                                |
| Live cloning (GPT-SoVITS v2)      | ~2-3 s        | Synthesizes from your text. Use for arbitrary unique strings.                        |
| Embedded TTS (`say` / `espeak-ng`) | <500 ms       | OS-default voice; what the daemon falls back to if no cloning is installed.          |

The daemon does the right routing automatically based on which
voice you point at.

---

## Verifying the integration

`voiceforge doctor` reports daemon status:

```bash
$ voiceforge doctor --json | jq '.checks[] | select(.name == "daemon")'
{
  "name": "daemon",
  "status": "ok",
  "detail": "running at /Users/me/.voiceforge/voiceforge.sock"
}
```

Quick smoke from any agent setup:

```bash
echo '{"event":"build_failed"}' | voiceforge hook
# -> voice speaks one of the configured build_failed lines
```

If the daemon isn't running, `voiceforge hook` exits 2 immediately
(no waiting for a threshold).

---

## Privacy

Every surface above runs entirely locally. The daemon listens on a
mode-0600 Unix socket in your home directory. The cloning runtime
talks to a local GPT-SoVITS v2 process. The pre-rendered pack WAVs
are read from local disk. **The only outbound network calls
voiceforge makes are**:

1. `pack install` (downloads tarballs from voice-forge-packs).
2. `install-cloning` (downloads the GPT-SoVITS model on first run).
3. `clone <name> <URL>` (uses yt-dlp to fetch the source — opt-out by
   downloading the audio yourself and passing a file path).

No telemetry, no analytics, no accounts.

---

## Reference: what each daemon event sounds like

The default `events.json` ships with these events. Add your own
freely.

| Event                | Default voice    | Sample line                                   |
| -------------------- | ---------------- | --------------------------------------------- |
| `build_failed`       | `angry_duck`     | "The build failed again."                     |
| `build_success`      | `hype_narrator`  | "Build completed successfully."               |
| `command_failed`     | `angry_duck`     | "Your command failed."                        |
| `command_succeeded`  | `hype_narrator`  | "Done."                                       |
| `git_commit`         | `sarcastic_goblin` | "Commit saved."                             |
| `daemon_alive`       | `tiny_robot`     | "VoiceForge is still running."                |

Add an event by editing `~/.voiceforge/rules/events.json`. Restart
the daemon to pick up changes.
