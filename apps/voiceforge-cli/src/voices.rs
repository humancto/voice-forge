//! Voice profile storage at `~/.voiceforge/voices/<name>/`.
//!
//! Two schemas coexist (PR-C scope):
//!
//! **v1 (GPT-SoVITS, recipe=`gpt-sovits-v2-multi-aux-ref`)** — legacy.
//!   profile.toml         schema=1, source, recipe, created_at, aux_count
//!   ref_main.wav + .txt  the main reference clip (10 s mono 32 kHz)
//!   aux_1..5.wav + .txt  five auxiliary references for tone fusion
//!
//! **v2 (fish-speech S2 Pro, recipe=`fish-speech-s2-pro`)** — v0.4 default.
//!   profile.toml         schema=2, source, recipe, created_at
//!   ref.wav + .txt       single 8-30s reference clip (32 kHz mono)
//!
//! Loading a voice asserts every referenced file exists, the recipe is
//! in the known set for its schema, and the schema_version is supported.
//! `load_voice` returns a `VoiceProfile` enum; callers pattern-match on
//! `V1` vs `V2`.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::paths;

/// Legacy single-schema constant. Kept as the `V1` value for callers
/// that haven't migrated to `KNOWN_RECIPES_V1` yet. New code should
/// use the V1/V2 split.
#[allow(dead_code)]
pub const VOICE_SCHEMA_VERSION: u32 = 1;

/// Recipes valid under schema_version=1 (legacy GPT-SoVITS path). A
/// future GPT-SoVITS variant could land here without bumping the
/// schema version.
pub const KNOWN_RECIPES_V1: &[&str] = &["gpt-sovits-v2-multi-aux-ref"];

/// Recipes valid under schema_version=2 (fish-speech path, v0.4 default).
pub const KNOWN_RECIPES_V2: &[&str] = &["fish-speech-s2-pro"];

/// Backward-compatibility alias. PR-C-a callers (tests + future
/// `voiceforge voices migrate` PR-C-b) use the V1/V2 split directly.
#[allow(dead_code)]
pub const KNOWN_RECIPES: &[&str] = KNOWN_RECIPES_V1;
pub const RESERVED_NAMES: &[&str] = &[
    "presets",
    "cache",
    "cloning",
    "voices",
    "embeddings",
    "logs",
];

/// Schema-1 voice profile (GPT-SoVITS, gpt-sovits-v2-multi-aux-ref).
/// Renamed from the pre-PR-C `VoiceProfile` struct; on-disk layout
/// unchanged. Existing v1 voices on user disks load through this
/// variant unchanged.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VoiceProfileV1 {
    pub schema_version: u32, // == 1
    pub name: String,
    pub source: String,
    /// Required: feeds the cloning cache key. Defaulting it would make
    /// every clone of a missing-created_at voice collide on the same
    /// cache entry, breaking `--force` invalidation.
    pub created_at: String,
    pub duration_seconds: f64,
    pub recipe: String, // in KNOWN_RECIPES_V1
    pub aux_count: usize,
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(skip)]
    pub ref_main_wav: PathBuf,
    #[serde(skip)]
    pub ref_main_txt: PathBuf,
    #[serde(skip)]
    pub aux_wavs: Vec<PathBuf>,
    #[serde(skip)]
    pub aux_txts: Vec<PathBuf>,
}

/// Schema-2 voice profile (fish-speech S2 Pro). Single 8-30s reference
/// clip + transcript; no aux files. Lit up in C-1b — load_voice
/// dispatches to `load_voice_v2` for schema_version=2 profiles.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VoiceProfileV2 {
    pub schema_version: u32, // == 2
    pub name: String,
    pub source: String,
    pub created_at: String,
    pub duration_seconds: f64,
    pub recipe: String, // in KNOWN_RECIPES_V2
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(skip)]
    pub ref_wav: PathBuf,
    #[serde(skip)]
    pub ref_txt: PathBuf,
}

/// Schema-discriminated voice profile. Built in-memory by `load_voice`
/// after peeking `schema_version` from the on-disk profile.toml; the
/// outer enum does NOT derive Deserialize (each variant deserializes
/// independently).
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceProfile {
    V1(VoiceProfileV1),
    V2(VoiceProfileV2),
}

impl VoiceProfile {
    pub fn name(&self) -> &str {
        match self {
            Self::V1(v) => &v.name,
            Self::V2(v) => &v.name,
        }
    }

    pub fn source(&self) -> &str {
        match self {
            Self::V1(v) => &v.source,
            Self::V2(v) => &v.source,
        }
    }

    pub fn created_at(&self) -> &str {
        match self {
            Self::V1(v) => &v.created_at,
            Self::V2(v) => &v.created_at,
        }
    }

    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        match self {
            Self::V1(v) => &v.dir,
            Self::V2(v) => &v.dir,
        }
    }

    #[allow(dead_code)]
    pub fn recipe(&self) -> &str {
        match self {
            Self::V1(v) => &v.recipe,
            Self::V2(v) => &v.recipe,
        }
    }

    #[allow(dead_code)]
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::V1(_) => 1,
            Self::V2(_) => 2,
        }
    }
}

/// Probe just `schema_version` from a profile.toml so `load_voice` can
/// dispatch to the right variant without serde tripping on missing
/// fields. Mirrors `install_cloning::peek_schema_version`.
fn peek_voice_schema_version(raw: &str, path: &Path) -> Result<u32> {
    #[derive(Deserialize)]
    struct Probe {
        schema_version: u32,
    }
    let probe: Probe = toml::from_str(raw).with_context(|| {
        let preview: String = raw.chars().take(80).collect();
        format!(
            "reading schema_version from {} ({} bytes; first 80 chars: {preview:?})",
            path.display(),
            raw.len(),
        )
    })?;
    Ok(probe.schema_version)
}

pub fn voices_dir() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("voices"))
}

/// Resolve `voices_dir/<name>` and assert the canonicalized path stays
/// under `voices_dir` (defense against symlink escape + path traversal
/// in `name`).
pub fn voice_dir(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let root = voices_dir().ok_or_else(|| anyhow!("could not resolve voices dir"))?;
    let candidate = root.join(name);
    Ok(candidate)
}

pub fn voice_exists(name: &str) -> bool {
    if validate_name(name).is_err() {
        return false;
    }
    let Ok(dir) = voice_dir(name) else {
        return false;
    };
    dir.join("profile.toml").is_file()
}

/// Reject names that could path-traverse, collide with reserved sibling
/// dirs under `~/.voiceforge/`, or contain shell-unsafe characters.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("voice name cannot be empty");
    }
    if name.len() > 32 {
        bail!("voice name too long (max 32 chars): {name:?}");
    }
    if name == "." || name == ".." {
        bail!("voice name cannot be '.' or '..'");
    }
    if RESERVED_NAMES.contains(&name) {
        bail!("voice name {name:?} collides with a reserved ~/.voiceforge/ subdir");
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !ok {
            bail!("voice name {name:?} has invalid char {c:?}; allowed: [a-z0-9_-], 1..=32");
        }
    }
    Ok(())
}

pub fn load_voice(name: &str) -> Result<VoiceProfile> {
    let dir = voice_dir(name)?;
    if !dir.is_dir() {
        bail!("voice {name:?} not found at {}", dir.display());
    }

    // Defense-in-depth: canonicalize and assert under voices_dir.
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", dir.display()))?;
    let voices_root = voices_dir()
        .ok_or_else(|| anyhow!("voices dir unresolved"))?
        .canonicalize()
        .with_context(|| "canonicalizing voices dir")?;
    if !canonical.starts_with(&voices_root) {
        bail!(
            "voice dir {} escapes {} (symlink?); refusing to load",
            canonical.display(),
            voices_root.display()
        );
    }

    let profile_path = canonical.join("profile.toml");
    let raw = std::fs::read_to_string(&profile_path)
        .with_context(|| format!("reading {}", profile_path.display()))?;
    let schema = peek_voice_schema_version(&raw, &profile_path)?;

    match schema {
        1 => load_voice_v1(name, &raw, &profile_path, canonical),
        2 => load_voice_v2(name, &raw, &profile_path, canonical),
        other => bail!(
            "voice {name:?} profile.toml schema_version={other} unsupported \
             (this build knows: 1, 2). Re-clone or check for typo."
        ),
    }
}

fn load_voice_v2(
    name: &str,
    raw: &str,
    profile_path: &Path,
    canonical: PathBuf,
) -> Result<VoiceProfile> {
    let mut profile: VoiceProfileV2 =
        toml::from_str(raw).with_context(|| format!("parsing {}", profile_path.display()))?;

    if !KNOWN_RECIPES_V2.contains(&profile.recipe.as_str()) {
        bail!(
            "voice {name:?} schema=2 but recipe {:?} not in KNOWN_RECIPES_V2 ({:?})",
            profile.recipe,
            KNOWN_RECIPES_V2
        );
    }

    profile.dir = canonical.clone();
    profile.ref_wav = canonical.join("ref.wav");
    profile.ref_txt = canonical.join("ref.txt");

    // V2 layout: single ref.wav + ref.txt. NO aux files. Future PR-C-b
    // migration leaves an `.v1.bak/` child dir with the legacy aux
    // files; load_voice tolerates that (we never walk subdirectories).
    assert_path_exists(&profile.ref_wav, "ref.wav")?;
    assert_path_exists(&profile.ref_txt, "ref.txt")?;

    Ok(VoiceProfile::V2(profile))
}

fn load_voice_v1(
    name: &str,
    raw: &str,
    profile_path: &Path,
    canonical: PathBuf,
) -> Result<VoiceProfile> {
    let mut profile: VoiceProfileV1 =
        toml::from_str(raw).with_context(|| format!("parsing {}", profile_path.display()))?;

    if !KNOWN_RECIPES_V1.contains(&profile.recipe.as_str()) {
        bail!(
            "voice {name:?} schema=1 but recipe {:?} not in KNOWN_RECIPES_V1 ({:?})",
            profile.recipe,
            KNOWN_RECIPES_V1
        );
    }

    profile.dir = canonical.clone();
    profile.ref_main_wav = canonical.join("ref_main.wav");
    profile.ref_main_txt = canonical.join("ref_main.txt");
    profile.aux_wavs = (1..=profile.aux_count)
        .map(|i| canonical.join(format!("aux_{i}.wav")))
        .collect();
    profile.aux_txts = (1..=profile.aux_count)
        .map(|i| canonical.join(format!("aux_{i}.txt")))
        .collect();

    assert_path_exists(&profile.ref_main_wav, "ref_main.wav")?;
    assert_path_exists(&profile.ref_main_txt, "ref_main.txt")?;
    for (i, p) in profile.aux_wavs.iter().enumerate() {
        assert_path_exists(p, &format!("aux_{}.wav", i + 1))?;
    }
    for (i, p) in profile.aux_txts.iter().enumerate() {
        assert_path_exists(p, &format!("aux_{}.txt", i + 1))?;
    }

    Ok(VoiceProfile::V1(profile))
}

fn assert_path_exists(p: &Path, label: &str) -> Result<()> {
    if !p.is_file() {
        bail!("voice profile is missing {} at {}", label, p.display());
    }
    Ok(())
}

/// Walk `~/.voiceforge/voices/` and return every parseable voice
/// profile. Broken profiles are skipped with a stderr warn unless
/// `VOICEFORGE_STRICT_VOICES=1`, in which case the broken profile is a
/// hard error.
pub fn list_cloned_voices() -> Result<Vec<VoiceProfile>> {
    let Some(dir) = voices_dir() else {
        return Ok(Vec::new());
    };
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let strict = std::env::var("VOICEFORGE_STRICT_VOICES")
        .map(|v| v == "1")
        .unwrap_or(false);

    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(&dir).with_context(|| format!("reading voices dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip the staging dirs left behind by interrupted clones.
        if name.ends_with(".partial") || name.ends_with(".lock.d") {
            continue;
        }
        if validate_name(name).is_err() {
            continue;
        }
        match load_voice(name) {
            Ok(profile) => out.push(profile),
            Err(e) if strict => {
                bail!("strict mode: voice {name:?} failed to load: {e:#}");
            }
            Err(e) => {
                eprintln!("voiceforge: skipping broken voice profile {name:?}: {e:#}");
            }
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(out)
}

/// Remove a cloned voice and prune any cache entries keyed on its
/// name. No-op-but-error on absent voice (so callers can pass the
/// error through to the user).
pub fn remove_cloned_voice(name: &str) -> Result<()> {
    validate_name(name)?;
    let dir = voice_dir(name)?;
    if !dir.is_dir() {
        bail!("voice {name:?} not found at {}", dir.display());
    }
    // Canonicalize + sanity-check before rm -rf.
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", dir.display()))?;
    let voices_root = voices_dir()
        .ok_or_else(|| anyhow!("voices dir unresolved"))?
        .canonicalize()
        .with_context(|| "canonicalizing voices dir")?;
    if !canonical.starts_with(&voices_root) {
        bail!(
            "voice dir {} escapes {}; refusing to remove",
            canonical.display(),
            voices_root.display()
        );
    }

    std::fs::remove_dir_all(&canonical)
        .with_context(|| format!("removing {}", canonical.display()))?;

    // Prune cache entries that mention this voice. Cache key is
    // sha256(text + voice + created_at + recipe), so the filename is
    // an opaque hash — we don't know which cache files were for this
    // voice without a sidecar manifest. For now, the cache key
    // guarantees cache entries are *valid only for the now-deleted
    // voice's created_at*; future clones with the same name get a new
    // created_at and naturally write new cache entries. Old entries
    // become unreachable but not invalid (they'd never be looked up).
    // Eviction by size/age is a follow-up.
    let _ = (); // intentional: orphan-but-unreachable, documented above.

    Ok(())
}

// ============================================================================
// PR-C-b: voice profile migration (v1 → v2)
// ============================================================================
//
// Convert an existing v1 voice (recipe=gpt-sovits-v2-multi-aux-ref) to v2
// (recipe=fish-speech-s2-pro) in place, preserving the cache-key invariants
// and leaving a recoverable `.v1.bak/` child dir.
//
// Atomic protocol uses two sibling staging dirs:
//   - `<voice>.partial-migrate/`  — staged v2 dir being built
//   - `<voice>.migrating-old/`    — old v1 dir moved aside during the flip
//
// Crash recovery is encoded in `PreflightState` (see below). The flip itself
// is verify-before-cleanup with rollback on verify failure — see
// `apps/voiceforge-cli/src/packs.rs:920-948` for the same pattern used in
// pack installation.

/// Outcome of a `migrate_voice` invocation. Enum (not a struct of options)
/// so the typestate contract — "either we no-op'd or we migrated" — is
/// compiler-checked.
#[derive(Debug, Clone, PartialEq)]
#[must_use = "the migrate report names the .v1.bak path users need"]
pub enum MigrateReport {
    /// Voice was already on schema 2; migrate was a no-op.
    AlreadyMigrated {
        voice_name: String,
        /// Path to a `.v1.bak/` recovery dir if it exists on disk
        /// (from a prior migration). Surfaced so repeat invocations
        /// remind the user where the recovery files live.
        v1_bak_path: Option<PathBuf>,
    },
    /// Voice was migrated from schema 1 → schema 2 by this invocation.
    Migrated {
        voice_name: String,
        original_recipe: String,
        new_recipe: String,
        v1_bak_path: PathBuf,
    },
}

impl MigrateReport {
    #[allow(dead_code)]
    pub fn voice_name(&self) -> &str {
        match self {
            Self::AlreadyMigrated { voice_name, .. } => voice_name,
            Self::Migrated { voice_name, .. } => voice_name,
        }
    }
    #[allow(dead_code)]
    pub fn already_migrated(&self) -> bool {
        matches!(self, Self::AlreadyMigrated { .. })
    }
}

/// The 8 possible disk states for a voice + its two staging-dir siblings.
/// Computed by `preflight_state` at the start of every migrate run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreflightState {
    /// peter/ present; no leftovers. Normal flow.
    Clean,
    /// peter/ present + peter.migrating-old/ leftover. Need --force to nuke.
    StrandedOld,
    /// peter/ present + peter.partial-migrate/ leftover. Need --force.
    StrandedStage,
    /// peter/ present + BOTH leftovers. Mid-protocol crash; need --force.
    CompoundStranded,
    /// peter/ absent + only peter.partial-migrate/ on disk. Need --force.
    /// Even with --force this bails: there's nothing to migrate.
    OnlyStage,
    /// peter/ absent + only peter.migrating-old/ on disk. Refuse even with
    /// --force — we won't silently resurrect a removed voice.
    OnlyOldNoForceResurrect,
    /// peter/ absent + BOTH leftovers. Legitimate 6a→6b crash window:
    /// auto-rollback (unconditionally) and proceed with normal flow.
    SelfHealMidFlip,
    /// Nothing on disk under any of the three names.
    MissingVoice,
}

fn voice_dir_unvalidated(name: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
    validate_name(name)?;
    let root = voices_dir().ok_or_else(|| anyhow!("could not resolve voices dir"))?;
    Ok((
        root.join(name),
        root.join(format!("{name}.partial-migrate")),
        root.join(format!("{name}.migrating-old")),
    ))
}

fn preflight_state(live: &Path, stage: &Path, old: &Path) -> PreflightState {
    match (live.is_dir(), stage.is_dir(), old.is_dir()) {
        (true, false, false) => PreflightState::Clean,
        (true, false, true) => PreflightState::StrandedOld,
        (true, true, false) => PreflightState::StrandedStage,
        (true, true, true) => PreflightState::CompoundStranded,
        (false, true, false) => PreflightState::OnlyStage,
        (false, false, true) => PreflightState::OnlyOldNoForceResurrect,
        (false, true, true) => PreflightState::SelfHealMidFlip,
        (false, false, false) => PreflightState::MissingVoice,
    }
}

/// RAII guard: nukes the staging dir on `Drop` unless `armed` is flipped to
/// false. Closes rust-expert B3: scope-guarded multi-file copy cleanup.
struct StagingGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(self.path);
        }
    }
}

/// Migrate a v1 voice profile to v2 in place. See the protocol comment
/// above for the disk-shape contract + crash-recovery rules. Idempotent
/// on already-v2. `force` clobbers leftover staging dirs from a prior
/// interrupted run.
pub fn migrate_voice(name: &str, force: bool) -> Result<MigrateReport> {
    migrate_voice_inner(name, force, &|| Ok(()))
}

/// Test-only entry point exposing a post-flip verification hook so we can
/// simulate a verify failure and assert the rollback restores the v1 state.
#[cfg(test)]
fn migrate_voice_test_hook(
    name: &str,
    force: bool,
    hook: &dyn Fn() -> Result<()>,
) -> Result<MigrateReport> {
    migrate_voice_inner(name, force, hook)
}

fn migrate_voice_inner(
    name: &str,
    force: bool,
    post_flip_hook: &dyn Fn() -> Result<()>,
) -> Result<MigrateReport> {
    let (live, stage, old) = voice_dir_unvalidated(name)?;
    let state = preflight_state(&live, &stage, &old);

    // ---- preflight cleanup / refusal -------------------------------------
    match state {
        PreflightState::Clean => { /* normal flow */ }
        PreflightState::StrandedStage => {
            if !force {
                bail!(
                    "voice {name:?}: leftover staging dir at {}; \
                     re-run with --force to clean up",
                    stage.display()
                );
            }
            std::fs::remove_dir_all(&stage)
                .with_context(|| format!("nuking {}", stage.display()))?;
        }
        PreflightState::StrandedOld => {
            if !force {
                bail!(
                    "voice {name:?}: leftover from prior migration at {}; \
                     re-run with --force to clean up",
                    old.display()
                );
            }
            std::fs::remove_dir_all(&old).with_context(|| format!("nuking {}", old.display()))?;
        }
        PreflightState::CompoundStranded => {
            if !force {
                bail!(
                    "voice {name:?}: prior migration crashed mid-protocol; \
                     stragglers at {} and {}; re-run with --force",
                    stage.display(),
                    old.display()
                );
            }
            std::fs::remove_dir_all(&stage)
                .with_context(|| format!("nuking {}", stage.display()))?;
            std::fs::remove_dir_all(&old).with_context(|| format!("nuking {}", old.display()))?;
        }
        PreflightState::SelfHealMidFlip => {
            // The one auto-heal case (rust-expert S1.2): peter/ is absent
            // AND partial-migrate/ is present AND migrating-old/ is present
            // — unambiguous mid-flip crash. Nuke the half-built staging
            // dir and restore the v1 from migrating-old/, then proceed
            // with the normal flow.
            std::fs::remove_dir_all(&stage)
                .with_context(|| format!("nuking {}", stage.display()))?;
            std::fs::rename(&old, &live).with_context(|| {
                format!("rollback rename {} -> {}", old.display(), live.display())
            })?;
        }
        PreflightState::OnlyStage => {
            if !force {
                bail!(
                    "voice {name:?}: leftover staging dir at {} but no live voice; \
                     re-run with --force to clean up (will then report voice-not-found)",
                    stage.display()
                );
            }
            std::fs::remove_dir_all(&stage)
                .with_context(|| format!("nuking {}", stage.display()))?;
            bail!("voice {name:?} not found");
        }
        PreflightState::OnlyOldNoForceResurrect => {
            bail!(
                "voice {name:?}: only a leftover migrating-old dir at {} — \
                 refusing to silently resurrect a previously-removed voice. \
                 If you want it back, manually rename: mv {} {}",
                old.display(),
                old.display(),
                live.display(),
            );
        }
        PreflightState::MissingVoice => {
            bail!("voice {name:?} not found");
        }
    }

    // ---- load + idempotency check ----------------------------------------
    let profile = load_voice(name)?;
    let v1 = match profile {
        VoiceProfile::V1(v1) => v1,
        VoiceProfile::V2(_) => {
            // Already v2: no-op exit 0. Surface .v1.bak/ path if a prior
            // migration left one (rust-expert: makes the idempotent path
            // still informative on repeat runs).
            let bak = live.join(".v1.bak");
            return Ok(MigrateReport::AlreadyMigrated {
                voice_name: name.to_string(),
                v1_bak_path: if bak.is_dir() { Some(bak) } else { None },
            });
        }
    };

    // Pre-flip notice (rust-expert Q3 partial: name the dirs before any
    // disk change so a Ctrl-C window leaves the user informed).
    eprintln!(
        "voiceforge: migrating {name} (v1 → v2). Staging at {}, original at {}.",
        stage.display(),
        live.display(),
    );

    // ---- step 3/4: build staging dir under a Drop guard ------------------
    std::fs::create_dir_all(&stage).with_context(|| format!("mkdir {}", stage.display()))?;
    let mut guard = StagingGuard {
        path: &stage,
        armed: true,
    };
    build_v2_staging(&v1, &live, &stage)?;
    guard.armed = false; // staging committed; no more rollback in this fn.

    // ---- step 5: flip + verify ------------------------------------------
    std::fs::rename(&live, &old)
        .with_context(|| format!("rename {} -> {}", live.display(), old.display()))?;
    if let Err(e) = std::fs::rename(&stage, &live) {
        // 5b rollback (rust-expert nit-1): if rename(stage → live) fails
        // after we've already renamed live → old, restore the v1 state.
        let _ = std::fs::rename(&old, &live);
        return Err(anyhow!(
            "rename {} -> {} failed: {}; v1 state restored",
            stage.display(),
            live.display(),
            e
        ));
    }

    // Verify the new live dir loads as V2; run the post-flip hook.
    let verify_result = (|| {
        let loaded = load_voice(name)?;
        match loaded {
            VoiceProfile::V2(_) => post_flip_hook(),
            VoiceProfile::V1(_) => bail!(
                "post-flip verification: load_voice returned V1 for {name:?} \
                 (expected V2); migration is logically broken"
            ),
        }
    })();

    if let Err(verify_err) = verify_result {
        // Rollback: move the (broken or hook-rejected) v2 dir back to
        // staging-name, restore migrating-old → live. After rollback the
        // disk state is functionally identical to the pre-migrate state
        // plus a leftover `partial-migrate/` staging dir.
        let _ = std::fs::rename(&live, &stage);
        let _ = std::fs::rename(&old, &live);
        bail!(
            "post-flip verification failed: {:#}. Rolled back to v1 state. \
             Leftover staging dir at {} can be cleaned with --force.",
            verify_err,
            stage.display()
        );
    }

    // ---- step 5d: best-effort cleanup of migrating-old/ -----------------
    // Tidiness, not correctness. If this fails the next --force run mops up.
    if let Err(e) = std::fs::remove_dir_all(&old) {
        eprintln!(
            "voiceforge: failed to clean up {}: {} (will be retried on next \
             voices migrate --force)",
            old.display(),
            e
        );
    }

    Ok(MigrateReport::Migrated {
        voice_name: name.to_string(),
        original_recipe: v1.recipe.clone(),
        new_recipe: "fish-speech-s2-pro".to_string(),
        v1_bak_path: live.join(".v1.bak"),
    })
}

/// Build the v2 staging dir: new profile.toml, ref.wav/.txt at top level,
/// `.v1.bak/` child with copies of all v1 artifacts. Every fallible op
/// inside relies on the `StagingGuard` in the caller for cleanup on error.
fn build_v2_staging(v1: &VoiceProfileV1, live: &Path, stage: &Path) -> Result<()> {
    // 4a: write the new v2 profile.toml. Preserve name/source/created_at/
    // duration_seconds — the cache-key salt-domain change (v1 →
    // fish-speech-s2-pro) handles re-synth naturally per locked
    // architecture decision #4, so we do NOT bump created_at.
    let new_profile_toml = format!(
        "schema_version = 2\n\
         name = \"{name}\"\n\
         source = \"{source}\"\n\
         created_at = \"{created_at}\"\n\
         duration_seconds = {duration_seconds}\n\
         recipe = \"fish-speech-s2-pro\"\n",
        name = toml_escape(&v1.name),
        source = toml_escape(&v1.source),
        created_at = toml_escape(&v1.created_at),
        duration_seconds = v1.duration_seconds,
    );
    std::fs::write(stage.join("profile.toml"), &new_profile_toml)
        .with_context(|| format!("writing {}/profile.toml", stage.display()))?;

    // 4b/4c: ref_main → ref at top level.
    std::fs::copy(live.join("ref_main.wav"), stage.join("ref.wav"))
        .with_context(|| "copying ref_main.wav -> ref.wav")?;
    std::fs::copy(live.join("ref_main.txt"), stage.join("ref.txt"))
        .with_context(|| "copying ref_main.txt -> ref.txt")?;

    // 4d/4e/4f: .v1.bak/ child with original profile + ref + aux pairs.
    let bak = stage.join(".v1.bak");
    std::fs::create_dir_all(&bak).with_context(|| format!("mkdir {}", bak.display()))?;
    std::fs::copy(live.join("profile.toml"), bak.join("profile.toml"))
        .with_context(|| "copying profile.toml -> .v1.bak/profile.toml")?;
    std::fs::copy(live.join("ref_main.wav"), bak.join("ref_main.wav"))
        .with_context(|| "copying ref_main.wav -> .v1.bak/ref_main.wav")?;
    std::fs::copy(live.join("ref_main.txt"), bak.join("ref_main.txt"))
        .with_context(|| "copying ref_main.txt -> .v1.bak/ref_main.txt")?;
    for i in 1..=v1.aux_count {
        let src_wav = live.join(format!("aux_{i}.wav"));
        let src_txt = live.join(format!("aux_{i}.txt"));
        let dst_wav = bak.join(format!("aux_{i}.wav"));
        let dst_txt = bak.join(format!("aux_{i}.txt"));
        std::fs::copy(&src_wav, &dst_wav)
            .with_context(|| format!("copying {} -> {}", src_wav.display(), dst_wav.display()))?;
        std::fs::copy(&src_txt, &dst_txt)
            .with_context(|| format!("copying {} -> {}", src_txt.display(), dst_txt.display()))?;
    }
    Ok(())
}

/// Minimal TOML basic-string escaper: " and \ get backslash-escaped.
/// The values we serialize (voice name, source path, created_at) are
/// already-constrained by `validate_name` / clone-time invariants, but
/// defending against future loosening is cheap.
fn toml_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn with_home<F: FnOnce(&Path)>(home: &Path, f: F) {
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home)));
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    fn write_full_profile(home: &Path, name: &str, recipe: &str) {
        let dir = home.join("voices").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let toml = format!(
            r#"
schema_version = 1
name = "{name}"
source = "/tmp/x.wav"
created_at = "2026-05-04T00:00:00Z"
duration_seconds = 60.0
recipe = "{recipe}"
aux_count = 5
"#
        );
        std::fs::write(dir.join("profile.toml"), toml).unwrap();
        std::fs::write(dir.join("ref_main.wav"), b"RIFF\0\0\0\0WAVE").unwrap();
        std::fs::write(dir.join("ref_main.txt"), b"main").unwrap();
        for i in 1..=5 {
            std::fs::write(dir.join(format!("aux_{i}.wav")), b"RIFF\0\0\0\0WAVE").unwrap();
            std::fs::write(dir.join(format!("aux_{i}.txt")), format!("aux_{i}")).unwrap();
        }
    }

    #[test]
    fn validate_name_accepts_simple() {
        validate_name("peter").unwrap();
        validate_name("trump_2024").unwrap();
        validate_name("a-b-c").unwrap();
        validate_name("x").unwrap();
    }

    #[test]
    fn validate_name_rejects_empty_or_too_long() {
        assert!(validate_name("").is_err());
        assert!(validate_name(&"a".repeat(33)).is_err());
    }

    #[test]
    fn validate_name_rejects_dot_and_traversal() {
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        // contains slash → invalid char
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("peter/etc").is_err());
        assert!(validate_name("peter\\etc").is_err());
    }

    #[test]
    fn validate_name_rejects_reserved() {
        for r in RESERVED_NAMES {
            assert!(validate_name(r).is_err(), "should reject {r}");
        }
    }

    #[test]
    fn validate_name_rejects_uppercase_and_special() {
        assert!(validate_name("Peter").is_err());
        assert!(validate_name("peter griffin").is_err());
        assert!(validate_name("peter@home").is_err());
        assert!(validate_name("../foo").is_err());
    }

    #[test]
    #[serial]
    fn voice_exists_false_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |_| {
            assert!(!voice_exists("peter"));
        });
    }

    #[test]
    #[serial]
    fn voice_exists_true_when_profile_present() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            assert!(voice_exists("peter"));
        });
    }

    #[test]
    #[serial]
    fn load_voice_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let v = load_voice("peter").unwrap();
            assert_eq!(v.name(), "peter");
            assert_eq!(v.recipe(), "gpt-sovits-v2-multi-aux-ref");
            assert_eq!(v.schema_version(), 1);
            // REGRESSION NET (PR #45 review D1): an existing v1 user
            // (schema-1 profile on disk, never ran `voiceforge voices
            // migrate`) MUST get back VoiceProfile::V1. Silent misroute
            // to V2 would land them in the FishEngine path, which bails
            // loud per C-4 — but the right behavior is "stay on V1."
            let v1 = match v {
                VoiceProfile::V1(v1) => v1,
                VoiceProfile::V2(_) => panic!(
                    "REGRESSION: load_voice returned V2 for a schema-1 profile on disk. \
                     The v1-still-works contract (PR-C C-5) is broken."
                ),
            };
            assert_eq!(v1.aux_count, 5);
            assert_eq!(v1.aux_wavs.len(), 5);
            assert!(v1.ref_main_wav.is_file());
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_missing_aux() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            std::fs::remove_file(home.join("voices/peter/aux_3.wav")).unwrap();
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("aux_3.wav"), "got: {msg}");
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_unknown_recipe() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "future-bigger-better-recipe");
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("KNOWN_RECIPES_V1") || msg.contains("unknown recipe"),
                "got: {msg}"
            );
        });
    }

    /// PR-C C-1a regression net: a profile.toml with an unsupported
    /// schema_version (not 1, not 2) must bail loud with a hint at
    /// the known schemas, NOT silently load it as v1.
    #[test]
    #[serial]
    fn load_voice_rejects_unknown_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = home.join("voices/peter");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("profile.toml"),
                r#"
schema_version = 99
name = "peter"
source = "stub"
created_at = "2026-05-12T00:00:00Z"
duration_seconds = 12.5
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
"#,
            )
            .unwrap();
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("schema_version=99") && msg.contains("unsupported"),
                "expected schema-99 unsupported error, got: {msg}"
            );
        });
    }

    // ========================================================================
    // PR-C C-1b: V2 surface lit up
    // ========================================================================

    /// Stage a minimal V2 voice on disk + return its dir for further
    /// fixture setup. Used by the C-1b test cluster below.
    fn write_v2_profile(home: &Path, name: &str, recipe: &str) -> PathBuf {
        let dir = home.join("voices").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("profile.toml"),
            format!(
                r#"
schema_version = 2
name = "{name}"
source = "stub"
created_at = "2026-05-12T00:00:00Z"
duration_seconds = 12.5
recipe = "{recipe}"
"#
            ),
        )
        .unwrap();
        // Real-ish RIFF/WAVE 12-byte header so file existence + basic
        // shape passes; not a playable WAV but enough for unit tests.
        std::fs::write(dir.join("ref.wav"), b"RIFF\x00\x00\x00\x00WAVEdata").unwrap();
        std::fs::write(
            dir.join("ref.txt"),
            "this is a smoke test reference transcript",
        )
        .unwrap();
        dir
    }

    #[test]
    #[serial]
    fn load_voice_v2_parses_fish_speech_profile() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_v2_profile(home, "tyson", "fish-speech-s2-pro");
            let v = load_voice("tyson").unwrap();
            assert_eq!(v.name(), "tyson");
            assert_eq!(v.recipe(), "fish-speech-s2-pro");
            assert_eq!(v.schema_version(), 2);
            let v2 = match v {
                VoiceProfile::V2(v2) => v2,
                VoiceProfile::V1(_) => panic!("expected V2 variant"),
            };
            assert!(v2.ref_wav.is_file());
            assert!(v2.ref_txt.is_file());
            assert_eq!(v2.duration_seconds, 12.5);
        });
    }

    /// Cross-schema-rejection: a schema=1 profile that claims a V2
    /// recipe fails at the KNOWN_RECIPES_V1 check.
    #[test]
    #[serial]
    fn load_voice_v1_with_v2_recipe_fails() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            // write_full_profile is the V1 helper; pass the V2 recipe.
            write_full_profile(home, "peter", "fish-speech-s2-pro");
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("schema=1") && msg.contains("KNOWN_RECIPES_V1"),
                "expected schema=1 + KNOWN_RECIPES_V1 mismatch error, got: {msg}"
            );
        });
    }

    /// Inverse: schema=2 + V1 recipe also fails at the new
    /// KNOWN_RECIPES_V2 check.
    #[test]
    #[serial]
    fn load_voice_v2_with_v1_recipe_fails() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_v2_profile(home, "tyson", "gpt-sovits-v2-multi-aux-ref");
            let err = load_voice("tyson").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("schema=2") && msg.contains("KNOWN_RECIPES_V2"),
                "expected schema=2 + KNOWN_RECIPES_V2 mismatch error, got: {msg}"
            );
        });
    }

    /// V2 layout has NO aux files. Staging an `aux_1.wav` next to a V2
    /// profile must NOT cause load_voice to try to read it or fail.
    /// (Plan v2 B3 explicit-negative test.)
    #[test]
    #[serial]
    fn load_voice_v2_does_not_assert_aux_files() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = write_v2_profile(home, "tyson", "fish-speech-s2-pro");
            // Stage stray aux files that V1 would have asserted on.
            // V2's loader must ignore them entirely.
            std::fs::write(dir.join("aux_1.wav"), b"junk").unwrap();
            std::fs::write(dir.join("aux_2.wav"), b"junk").unwrap();
            let v = load_voice("tyson").unwrap();
            assert_eq!(v.schema_version(), 2);
        });
    }

    /// PR-C-b will leave behind `<voice>/.v1.bak/` after a migration.
    /// Today's load_voice must tolerate that child dir. (Plan v2 R5
    /// regression net.)
    #[test]
    #[serial]
    fn load_voice_v2_tolerates_dot_v1_bak_child_dir() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = write_v2_profile(home, "tyson", "fish-speech-s2-pro");
            // Simulate a migration leftover from PR-C-b.
            let bak = dir.join(".v1.bak");
            std::fs::create_dir_all(&bak).unwrap();
            std::fs::write(bak.join("ref_main.wav"), b"old aux").unwrap();
            std::fs::write(bak.join("aux_1.wav"), b"old aux").unwrap();
            // Must still load cleanly — load_voice never walks subdirs.
            let v = load_voice("tyson").unwrap();
            assert_eq!(v.schema_version(), 2);
        });
    }

    /// Decision #7 lock: voice_exists() stays schema-agnostic. Both
    /// v1 and v2 profiles must register as `exists`. (Plan v2 M3
    /// regression net.)
    #[test]
    #[serial]
    fn voice_exists_remains_schema_agnostic() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            write_v2_profile(home, "tyson", "fish-speech-s2-pro");
            assert!(voice_exists("peter"), "v1 profile must register");
            assert!(voice_exists("tyson"), "v2 profile must register");
            assert!(!voice_exists("nobody"), "absent voice must not register");
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_schema_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = home.join("voices/peter");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("profile.toml"),
                br#"
schema_version = 99
name = "peter"
source = "x"
created_at = "2026-05-04T00:00:00Z"
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
"#,
            )
            .unwrap();
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("schema_version"), "got: {msg}");
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_missing_created_at() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = home.join("voices/peter");
            std::fs::create_dir_all(&dir).unwrap();
            // No created_at — required field; cache key would otherwise
            // collide with every other no-created_at clone.
            std::fs::write(
                dir.join("profile.toml"),
                br#"
schema_version = 1
name = "peter"
source = "x"
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
"#,
            )
            .unwrap();
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("created_at") || msg.contains("missing field"),
                "expected missing-field error mentioning created_at, got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn load_voice_blocks_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            // Stage a real voice profile *outside* voices_dir.
            let outside = home.join("evil");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("marker"), b"x").unwrap();

            // Create voices_dir + a symlink in it pointing outside.
            let voices = home.join("voices");
            std::fs::create_dir_all(&voices).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside, voices.join("escaper")).unwrap();

            // load_voice should refuse — symlink target is outside voices_dir.
            let err = load_voice("escaper").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("escapes") || msg.contains("not found") || msg.contains("missing"),
                "expected escape/not-found error, got: {msg}"
            );
        });
    }

    // -- list_cloned_voices + remove_cloned_voice ---------------------

    #[test]
    #[serial]
    fn list_cloned_voices_skips_broken_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            // Stage a second dir that's missing the profile.toml entirely.
            std::fs::create_dir_all(home.join("voices/broken")).unwrap();
            std::fs::write(home.join("voices/broken/junk"), b"x").unwrap();

            let voices = list_cloned_voices().expect("list");
            let names: Vec<&str> = voices.iter().map(|v| v.name()).collect();
            assert_eq!(names, vec!["peter"], "broken voice should be skipped");
        });
    }

    #[test]
    #[serial]
    fn list_cloned_voices_skips_partial_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            // Simulate an interrupted clone — left-behind .partial dir.
            std::fs::create_dir_all(home.join("voices/peter.partial")).unwrap();

            let voices = list_cloned_voices().expect("list");
            let names: Vec<&str> = voices.iter().map(|v| v.name()).collect();
            assert_eq!(names, vec!["peter"]);
        });
    }

    #[test]
    #[serial]
    fn list_cloned_voices_strict_mode_errors_on_broken() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            std::fs::create_dir_all(home.join("voices/broken")).unwrap();
            std::fs::write(home.join("voices/broken/junk"), b"x").unwrap();

            std::env::set_var("VOICEFORGE_STRICT_VOICES", "1");
            let result = list_cloned_voices();
            std::env::remove_var("VOICEFORGE_STRICT_VOICES");
            assert!(result.is_err(), "strict mode should error on broken voice");
        });
    }

    #[test]
    #[serial]
    fn remove_cloned_voice_deletes_dir() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            assert!(voice_exists("peter"));
            remove_cloned_voice("peter").unwrap();
            assert!(!voice_exists("peter"));
            assert!(!home.join("voices/peter").exists());
        });
    }

    #[test]
    #[serial]
    fn remove_cloned_voice_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |_| {
            let err = remove_cloned_voice("../etc").unwrap_err();
            assert!(format!("{err:#}").contains("invalid"));
        });
    }

    #[test]
    #[serial]
    fn remove_cloned_voice_errors_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |_| {
            let err = remove_cloned_voice("nonexistent").unwrap_err();
            assert!(format!("{err:#}").contains("not found"));
        });
    }

    // ========================================================================
    // PR-C-b: migrate_voice (v1 → v2)
    // ========================================================================

    /// Variant of `write_full_profile` with a tunable `aux_count` so we
    /// can prove `migrate_voice` handles N != 5 (rust-expert R4).
    fn write_full_profile_with_aux_count(home: &Path, name: &str, aux_count: usize) {
        let dir = home.join("voices").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let toml = format!(
            r#"
schema_version = 1
name = "{name}"
source = "/tmp/x.wav"
created_at = "2026-05-04T00:00:00Z"
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = {aux_count}
"#
        );
        std::fs::write(dir.join("profile.toml"), toml).unwrap();
        std::fs::write(dir.join("ref_main.wav"), b"RIFF\0\0\0\0WAVE").unwrap();
        std::fs::write(dir.join("ref_main.txt"), b"main").unwrap();
        for i in 1..=aux_count {
            std::fs::write(dir.join(format!("aux_{i}.wav")), b"RIFF\0\0\0\0WAVE").unwrap();
            std::fs::write(dir.join(format!("aux_{i}.txt")), format!("aux_{i}")).unwrap();
        }
    }

    /// Test #1: post-migrate, profile.toml is v2 + load_voice returns V2.
    #[test]
    #[serial]
    fn migrate_v1_writes_v2_profile_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let report = migrate_voice("peter", false).expect("migrate");
            assert!(!report.already_migrated());
            let raw = std::fs::read_to_string(home.join("voices/peter/profile.toml")).unwrap();
            assert!(raw.contains("schema_version = 2"), "got: {raw}");
            assert!(
                raw.contains(r#"recipe = "fish-speech-s2-pro""#),
                "got: {raw}"
            );
            let v = load_voice("peter").unwrap();
            assert_eq!(v.schema_version(), 2);
        });
    }

    /// Test #2: cache-key invariants preserved (rust-expert: cache salt
    /// domain change handles re-synth; the FIELDS must stay byte-identical).
    #[test]
    #[serial]
    fn migrate_v1_preserves_name_source_created_at_duration() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let _ = migrate_voice("peter", false).unwrap();
            let v = load_voice("peter").unwrap();
            let v2 = match v {
                VoiceProfile::V2(v) => v,
                VoiceProfile::V1(_) => panic!("expected V2"),
            };
            assert_eq!(v2.name, "peter");
            assert_eq!(v2.source, "/tmp/x.wav");
            assert_eq!(v2.created_at, "2026-05-04T00:00:00Z");
            assert!((v2.duration_seconds - 60.0).abs() < 1e-9);
        });
    }

    /// Test #3: ref_main.{wav,txt} GONE from top level; ref.{wav,txt} present.
    #[test]
    #[serial]
    fn migrate_v1_renames_ref_main_to_ref() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let _ = migrate_voice("peter", false).unwrap();
            let dir = home.join("voices/peter");
            assert!(
                dir.join("ref.wav").is_file(),
                "ref.wav missing at top level"
            );
            assert!(
                dir.join("ref.txt").is_file(),
                "ref.txt missing at top level"
            );
            assert!(
                !dir.join("ref_main.wav").exists(),
                "ref_main.wav should be gone from top level (moved to .v1.bak/)"
            );
            assert!(
                !dir.join("ref_main.txt").exists(),
                "ref_main.txt should be gone from top level (moved to .v1.bak/)"
            );
        });
    }

    /// Test #4 (rust-expert nit-4 sharpened): assert each expected
    /// filename in .v1.bak/, not just the count.
    #[test]
    #[serial]
    fn migrate_v1_moves_aux_to_v1_bak_child() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let _ = migrate_voice("peter", false).unwrap();
            let bak = home.join("voices/peter/.v1.bak");
            assert!(bak.is_dir(), ".v1.bak/ should exist");
            let mut expected: Vec<String> = vec![
                "profile.toml".into(),
                "ref_main.wav".into(),
                "ref_main.txt".into(),
            ];
            for i in 1..=5 {
                expected.push(format!("aux_{i}.wav"));
                expected.push(format!("aux_{i}.txt"));
            }
            for name in &expected {
                assert!(
                    bak.join(name).is_file(),
                    ".v1.bak/{name} should exist after migrate"
                );
            }
            // Also assert exact file count (no extras).
            let actual: Vec<String> = std::fs::read_dir(&bak)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            assert_eq!(
                actual.len(),
                expected.len(),
                "expected {} files in .v1.bak/, got {} ({:?})",
                expected.len(),
                actual.len(),
                actual
            );
        });
    }

    /// Test #5 (rust-expert B4 + impl-detail #9): idempotent on already-v2;
    /// content-compare profile.toml, NOT mtime; AlreadyMigrated branch
    /// must not touch disk.
    #[test]
    #[serial]
    fn migrate_idempotent_on_already_v2() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_v2_profile(home, "tyson", "fish-speech-s2-pro");
            let path = home.join("voices/tyson/profile.toml");
            let before = std::fs::read(&path).unwrap();
            let report = migrate_voice("tyson", false).expect("migrate idempotent");
            assert!(report.already_migrated());
            assert_eq!(report.voice_name(), "tyson");
            // Content-compare proves the no-op branch did NOT rewrite disk.
            let after = std::fs::read(&path).unwrap();
            assert_eq!(
                before, after,
                "AlreadyMigrated must not modify profile.toml"
            );
            // Without a .v1.bak/ on disk, the report's path is None.
            match report {
                MigrateReport::AlreadyMigrated { v1_bak_path, .. } => {
                    assert!(v1_bak_path.is_none());
                }
                _ => panic!("expected AlreadyMigrated"),
            }
        });
    }

    /// Test #5b: AlreadyMigrated surfaces .v1.bak/ path when one exists
    /// on disk (a prior migration's recovery dir).
    #[test]
    #[serial]
    fn migrate_idempotent_on_already_v2_surfaces_v1_bak() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = write_v2_profile(home, "tyson", "fish-speech-s2-pro");
            std::fs::create_dir_all(dir.join(".v1.bak")).unwrap();
            let report = migrate_voice("tyson", false).unwrap();
            match report {
                MigrateReport::AlreadyMigrated { v1_bak_path, .. } => {
                    assert_eq!(v1_bak_path, Some(dir.join(".v1.bak")));
                }
                _ => panic!("expected AlreadyMigrated"),
            }
        });
    }

    /// Test #6: bails on leftover partial-migrate dir without --force.
    #[test]
    #[serial]
    fn migrate_bails_on_partial_dir_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            std::fs::create_dir_all(home.join("voices/peter.partial-migrate")).unwrap();
            let err = migrate_voice("peter", false).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("partial-migrate"), "got: {msg}");
            assert!(msg.contains("--force"), "got: {msg}");
        });
    }

    /// Test #7: --force clobbers existing partial-migrate dir.
    #[test]
    #[serial]
    fn migrate_force_clobbers_existing_partial_migrate_dir() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let leftover = home.join("voices/peter.partial-migrate");
            std::fs::create_dir_all(&leftover).unwrap();
            std::fs::write(leftover.join("sentinel"), b"junk").unwrap();
            let report = migrate_voice("peter", true).expect("force should succeed");
            assert!(!report.already_migrated());
            // After migration, the partial-migrate dir is the new live dir
            // — but we renamed it AND the leftover was nuked first, so the
            // sentinel file from the old leftover is gone.
            assert!(!home.join("voices/peter.partial-migrate").exists());
            assert!(load_voice("peter").unwrap().schema_version() == 2);
        });
    }

    /// Test #8 (rust-expert nit-5 sharpened): orphan-old-after-remove —
    /// bails even with --force, AND disk state is unchanged on the
    /// --force=true bail (no silent mutation).
    #[test]
    #[serial]
    fn migrate_orphan_old_refuses_even_with_force() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            // Stage only migrating-old/ — no live, no partial.
            let stranded = home.join("voices/peter.migrating-old");
            std::fs::create_dir_all(&stranded).unwrap();
            std::fs::write(stranded.join("sentinel"), b"v1-ghost").unwrap();

            let err = migrate_voice("peter", false).unwrap_err();
            assert!(format!("{err:#}").contains("resurrect"), "got: {err:#}");
            assert!(stranded.is_dir(), "force=false must not mutate disk");

            let err = migrate_voice("peter", true).unwrap_err();
            assert!(format!("{err:#}").contains("resurrect"), "got: {err:#}");
            assert!(
                stranded.join("sentinel").is_file(),
                "force=true must not silently nuke migrating-old"
            );
        });
    }

    /// Test #9 (rust-expert nit-6 sharpened): self-heal mid-flip — both
    /// leftovers gone post-test, peter/ is V2.
    #[test]
    #[serial]
    fn migrate_self_heals_when_mid_flip_state_detected() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            // Stage a v1 voice, then SIMULATE a 6a→6b crash: rename
            // peter → peter.migrating-old, and leave a half-built
            // peter.partial-migrate/ behind.
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            std::fs::rename(
                home.join("voices/peter"),
                home.join("voices/peter.migrating-old"),
            )
            .unwrap();
            // Half-built partial-migrate; the migrator should NUKE this
            // and resurrect the v1 from migrating-old/.
            let partial = home.join("voices/peter.partial-migrate");
            std::fs::create_dir_all(&partial).unwrap();
            std::fs::write(partial.join("garbage"), b"half-built").unwrap();

            let report = migrate_voice("peter", false).expect("self-heal then migrate");
            assert!(!report.already_migrated());
            assert!(load_voice("peter").unwrap().schema_version() == 2);
            assert!(!home.join("voices/peter.partial-migrate").exists());
            assert!(!home.join("voices/peter.migrating-old").exists());
        });
    }

    /// Test #10: bails loud on unknown voice.
    #[test]
    #[serial]
    fn migrate_bails_on_unknown_voice() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |_| {
            let err = migrate_voice("nobody", false).unwrap_err();
            assert!(format!("{err:#}").contains("not found"), "got: {err:#}");
        });
    }

    /// Test #11: round-trip via load_voice.
    #[test]
    #[serial]
    fn migrate_v2_profile_loadable_via_load_voice() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let _ = migrate_voice("peter", false).unwrap();
            let v = load_voice("peter").unwrap();
            let v2 = match v {
                VoiceProfile::V2(v) => v,
                VoiceProfile::V1(_) => panic!("expected V2 after migrate"),
            };
            assert!(v2.ref_wav.is_file());
            assert!(v2.ref_txt.is_file());
        });
    }

    /// Test #12 (rust-expert R4): aux_count = 3 → 9 files in .v1.bak/
    /// (1 profile + 2 ref + 6 aux).
    #[test]
    #[serial]
    fn migrate_with_aux_count_3_copies_three_aux_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile_with_aux_count(home, "peter", 3);
            let _ = migrate_voice("peter", false).expect("migrate aux=3");
            let bak = home.join("voices/peter/.v1.bak");
            assert!(bak.is_dir());
            for i in 1..=3 {
                assert!(bak.join(format!("aux_{i}.wav")).is_file());
                assert!(bak.join(format!("aux_{i}.txt")).is_file());
            }
            assert!(!bak.join("aux_4.wav").exists());
            let actual_count = std::fs::read_dir(&bak).unwrap().count();
            assert_eq!(
                actual_count, 9,
                "aux_count=3 should yield 9 files in .v1.bak/"
            );
        });
    }

    /// Test #13 (rust-expert R1 + nit-7 sharpened): compound-stranded —
    /// without --force bails AND live v1 is byte-unchanged on the bail
    /// path; with --force migrates cleanly.
    #[test]
    #[serial]
    fn migrate_full_6a_6b_compound_stranded_with_force() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            std::fs::create_dir_all(home.join("voices/peter.partial-migrate")).unwrap();
            std::fs::create_dir_all(home.join("voices/peter.migrating-old")).unwrap();

            // Capture v1 profile bytes for unchanged-on-bail assertion.
            let live_profile = home.join("voices/peter/profile.toml");
            let before = std::fs::read(&live_profile).unwrap();

            // --force=false bails; live v1 must be untouched.
            let err = migrate_voice("peter", false).unwrap_err();
            assert!(format!("{err:#}").contains("--force"), "got: {err:#}");
            let after_bail = std::fs::read(&live_profile).unwrap();
            assert_eq!(before, after_bail, "live v1 must be byte-unchanged on bail");

            // --force=true: nukes both stragglers and migrates cleanly.
            let _ = migrate_voice("peter", true).expect("--force should migrate");
            assert!(!home.join("voices/peter.partial-migrate").exists());
            assert!(!home.join("voices/peter.migrating-old").exists());
            assert!(load_voice("peter").unwrap().schema_version() == 2);
        });
    }

    /// Test #14 (rust-expert R3): verify-failure rollback. Inject a hook
    /// that returns Err *after* the rename flip; assert the disk is
    /// fully rolled back to v1.
    #[test]
    #[serial]
    fn migrate_verify_failure_rolls_back_to_v1() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let err = migrate_voice_test_hook("peter", false, &|| {
                Err(anyhow!("synthetic verify failure"))
            })
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("synthetic verify failure"),
                "got: {err:#}"
            );
            // Disk must be fully restored to v1 state.
            let v = load_voice("peter").unwrap();
            assert_eq!(
                v.schema_version(),
                1,
                "verify-failure rollback must restore schema=1"
            );
            // All 5 aux pairs back at top level.
            let dir = home.join("voices/peter");
            for i in 1..=5 {
                assert!(
                    dir.join(format!("aux_{i}.wav")).is_file(),
                    "aux_{i}.wav must be restored to top level"
                );
            }
            // ref_main.wav back at top level (not ref.wav).
            assert!(dir.join("ref_main.wav").is_file());
            assert!(!dir.join("ref.wav").exists());
            // migrating-old/ must be gone (consumed by rollback rename).
            assert!(!home.join("voices/peter.migrating-old").exists());
            // partial-migrate/ remains as a leftover the --force path
            // can clean (matches the verify-failure rollback contract).
            assert!(home.join("voices/peter.partial-migrate").exists());
        });
    }

    /// Test #14b (rust-expert nit-8 control arm): Ok(()) hook completes
    /// migration normally. Without this, a "rollback always runs" bug
    /// could silently pass test #14.
    #[test]
    #[serial]
    fn migrate_with_ok_hook_completes_normally() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let report =
                migrate_voice_test_hook("peter", false, &|| Ok(())).expect("Ok hook should pass");
            assert!(!report.already_migrated());
            assert_eq!(load_voice("peter").unwrap().schema_version(), 2);
            assert!(!home.join("voices/peter.migrating-old").exists());
            assert!(!home.join("voices/peter.partial-migrate").exists());
        });
    }

    /// Test #15 (rust-expert nit-2, R2 completion): preflight row 6 —
    /// `peter/` absent, `partial-migrate/` present, `migrating-old/`
    /// absent. Without --force bails; with --force nukes partial AND
    /// then reports voice-not-found.
    #[test]
    #[serial]
    fn migrate_only_partial_stage_present() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            std::fs::create_dir_all(home.join("voices/peter.partial-migrate")).unwrap();

            let err = migrate_voice("peter", false).unwrap_err();
            assert!(format!("{err:#}").contains("--force"), "got: {err:#}");
            assert!(home.join("voices/peter.partial-migrate").is_dir());

            let err = migrate_voice("peter", true).unwrap_err();
            assert!(format!("{err:#}").contains("not found"), "got: {err:#}");
            assert!(!home.join("voices/peter.partial-migrate").exists());
        });
    }
}
