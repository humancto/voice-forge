# Plan: audio-ingest pipeline (ROADMAP 2.2) — REVISED post rust-expert review

## Goal

Take any audio source (wav/mp3/m4a/ogg/flac/webm/aiff) and emit a canonical
XTTS-ready WAV: **22050 Hz, mono, 16-bit PCM, 10–60 s duration**.

Foundation for `voiceforge clone` (2.5), `voiceforge record` (2.4), and the
URL ingest in 2.3.

## Out of scope

- URL-as-source via yt-dlp wrapper — 2.3 (next branch).
- XTTS embedding generation — 2.5 / 3.2.
- Mic capture — 2.4.
- Loudness normalization — deferred to **ROADMAP 2.2.1** (filed in this PR).
- Silence detection (reject all-silent input) — deferred to **2.2.1**.

## Files

### New

- `scripts/fetch_fixtures.sh` — idempotent yt-dlp + ffmpeg-trim of the Peter
  Griffin clip into `tests/fixtures/peter_griffin.wav` (**20s mono 22050Hz
  16-bit PCM**, comfortably inside the 10–60s contract). No-op when fixture
  already exists. Errors with clear instructions if yt-dlp/ffmpeg missing.
- `apps/voiceforge-cli/src/ingest.rs` — pure ingest module:
  - `pub struct IngestConfig { target_sample_rate: u32, target_channels: u16,
min_seconds: f64, max_seconds: f64, ffmpeg_timeout: Duration }` with
    `Default` → `22050 / 1 / 10.0 / 60.0 / 30s`.
  - `pub struct IngestReport { sample_rate: u32, channels: u16,
bits_per_sample: u16, codec: String, duration_seconds: f64 }`.
  - `pub struct AudioProbe { sample_rate: u32, channels: u16,
bits_per_sample: u16, codec: String, duration_seconds: f64 }` (same
    shape; type-distinct so callers can't confuse "what we got" with
    "what we want"). Both deserialize from `ffprobe -of json -show_streams
-show_format` via private `serde` structs.
  - `pub fn probe(path: &Path) -> anyhow::Result<AudioProbe>` — runs
    `ffprobe -v error -of json -show_streams -show_format -- <path>`,
    returns parsed audio-stream metadata. Errors with `.context()` carrying
    the path and a "did ffprobe install correctly?" hint when the binary's
    missing.
  - `pub fn ingest(input: &Path, output: &Path, cfg: &IngestConfig)
-> anyhow::Result<IngestReport>` — runs `ffmpeg -y -i <input> -ar
<sr> -ac <ch> -acodec pcm_s16le -t <max_seconds> -- <output>`
    with the `--` to defuse leading-dash ambiguity. Wraps the spawn in a
    manual timeout (kill on expiry). Then `probe(output)` and asserts
    duration ∈ [min, max], else returns an `anyhow!` error whose message
    embeds the actual + bounds.
  - All errors use `anyhow::Result` with `.context()` to match the existing
    codebase style. No new error enum.
- `apps/voiceforge-cli/tests/audio_ingest.rs` — integration tests (see
  Verify section for the SKIP/REQUIRE-FIXTURES contract):
  - `ingest_fixture_yields_canonical_wav` — runs ingest on
    `tests/fixtures/peter_griffin.wav` to a tempdir, asserts
    `sample_rate == 22050`, `channels == 1`, `bits_per_sample == 16`,
    `codec == "pcm_s16le"`, duration ∈ `[9.5, 60.5]`. **SKIP semantics
    below.**
  - `ingest_rejects_too_short` — generates a 1s 22050/1/16-bit WAV via
    `hound` (new dev-dep) into a tempdir, asserts ingest returns an error
    whose message contains `"duration"` and `"1."` (rough match — we own
    the format string).
  - `ingest_rejects_missing_input` — passes a non-existent path, asserts
    error message contains `"No such file"` or similar from `.context`.
  - `ingest_rejects_dash_prefixed_input` — creates `-rf foo.wav` in
    tempdir and tries to ingest it; with `--` separator this should
    succeed (or fail-on-not-an-audio-file), NOT be parsed as an ffmpeg
    flag. Regression test for the injection vector.

### Modified

- `apps/voiceforge-cli/src/main.rs` — `mod ingest;`, add
  `Commands::Ingest { input: PathBuf, output: PathBuf }` calling
  `ingest::ingest(&input, &output, &Default::default())` and printing the
  report.
- `apps/voiceforge-cli/Cargo.toml` — add **dev-dep** `hound = "3"` for
  test-only WAV synthesis. Add **dep** `serde` already exists. No new
  runtime deps.
- `.gitignore` — confirm presence of these lines (already there from initial
  commit, just verifying):
  ```
  tests/fixtures/*.wav
  tests/fixtures/*.mp3
  tests/fixtures/*.m4a
  tests/fixtures/*.ogg
  tests/fixtures/*.flac
  tests/fixtures/*.aiff
  tests/fixtures/*.pt
  ```
- `ROADMAP.md` — append item **2.2.1**: silence-rejection + loudness
  normalization (separate PR).

## Verify (real, not "trust CI")

1. `bash scripts/fetch_fixtures.sh` — fetches + trims; second run is no-op
   (assert by checking mtime unchanged).
2. `cargo test --manifest-path apps/voiceforge-cli/Cargo.toml audio_ingest`
   — all four tests pass locally with the fixture present.
3. Delete `tests/fixtures/peter_griffin.wav`, rerun `cargo test
audio_ingest` — `ingest_fixture_yields_canonical_wav` SKIPs (prints
   `"SKIP: fixture missing, run scripts/fetch_fixtures.sh"` and returns
   `Ok(())`). The other three still pass. Suite reports green.
4. `VOICEFORGE_REQUIRE_FIXTURES=1 cargo test audio_ingest` (with fixture
   still missing) — `ingest_fixture_yields_canonical_wav` **fails** with a
   clear "fixture required but missing" message. This is the CI gate that
   keeps the skip path honest.
5. `cargo run -- ingest tests/fixtures/peter_griffin.wav /tmp/out.wav`
   then `ffprobe -of json -show_streams /tmp/out.wav` — confirm 22050/1
   pcm_s16le visually.

## Risks (non-blocking)

- **ffmpeg flag drift across versions.** Mitigation: pin to widely-supported
  flags only.
- **macOS `say` AIFF inputs** — added `aiff` to accepted extensions; ffmpeg
  decodes natively, no special handling needed.
- **Test slowness** — `hound` synth + 4 ffmpeg invocations ≈ <1s total.
- **Cross-platform ffmpeg path discovery on Windows** — deferred. Trust
  `PATH` for now; revisit when CI matrix adds Windows.

## Atomic commits

1. `chore: add tests/fixtures + fetch_fixtures.sh + ROADMAP 2.2.1 follow-up`
2. `feat(ingest): add audio-ingest module (ffmpeg shell-out, ffprobe json)`
3. `feat(cli): wire 'voiceforge ingest' subcommand`
4. `test(ingest): integration tests against Peter Griffin fixture`
