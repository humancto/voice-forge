use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct IngestConfig {
    pub target_sample_rate: u32,
    pub target_channels: u16,
    pub min_seconds: f64,
    pub max_seconds: f64,
    pub ffmpeg_timeout: Duration,
    /// Apply EBU R128 loudnorm during ingest (ROADMAP 2.2.1). Default
    /// `true` so all clones land at broadcast-friendly level (no
    /// whispered Trump or screaming Peter). Tests can disable to keep
    /// signal arithmetic deterministic.
    pub apply_loudnorm: bool,
    /// Reject post-encode WAVs whose mean absolute amplitude is below
    /// this fraction of full scale. 0.005 ≈ -46 dBFS — well below
    /// even quiet speech, so anything that trips this is genuinely
    /// silent or near-silent. ROADMAP 2.2.1.
    pub silence_min_mean_abs: f32,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            // GPT-SoVITS v2 trains and infers at 32 kHz. Earlier we used
            // 22_050 (XTTS native), which forced the model to internally
            // resample with a non-integer ratio (22050→32000 = 1.4512×) —
            // producing phasing / "from a well" comb-filter artifacts in
            // the cloned output. Match the model rate exactly.
            target_sample_rate: 32_000,
            target_channels: 1,
            min_seconds: 10.0,
            max_seconds: 60.0,
            ffmpeg_timeout: Duration::from_secs(30),
            apply_loudnorm: true,
            silence_min_mean_abs: 0.005,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IngestReport {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub codec: String,
    pub duration_seconds: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioProbe {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub codec: String,
    pub duration_seconds: f64,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    streams: Vec<FfprobeStream>,
    format: FfprobeFormat,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: String,
    codec_name: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u16>,
    bits_per_sample: Option<u16>,
    bits_per_raw_sample: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

pub fn probe(path: &Path) -> Result<AudioProbe> {
    if !path.exists() {
        bail!("audio source not found: {}", path.display());
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-of",
            "json",
            "-show_streams",
            "-show_format",
            "--",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            "failed to invoke ffprobe (is it installed and on PATH? `brew install ffmpeg`)"
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe failed for {}: {}", path.display(), stderr.trim());
    }

    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("could not parse ffprobe JSON for {}", path.display()))?;

    let audio = parsed
        .streams
        .into_iter()
        .find(|s| s.codec_type == "audio")
        .ok_or_else(|| anyhow!("no audio stream in {}", path.display()))?;

    let sample_rate = audio
        .sample_rate
        .as_deref()
        .ok_or_else(|| anyhow!("ffprobe did not report a sample_rate"))?
        .parse::<u32>()
        .context("could not parse ffprobe sample_rate")?;

    let channels = audio
        .channels
        .ok_or_else(|| anyhow!("ffprobe did not report a channel count"))?;

    let bits_per_sample = audio.bits_per_sample.unwrap_or(0);
    let bits_per_sample = if bits_per_sample > 0 {
        bits_per_sample
    } else {
        audio
            .bits_per_raw_sample
            .as_deref()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0)
    };

    let codec = audio
        .codec_name
        .ok_or_else(|| anyhow!("ffprobe did not report a codec_name"))?;

    let duration_seconds = parsed
        .format
        .duration
        .as_deref()
        .ok_or_else(|| anyhow!("ffprobe did not report a duration"))?
        .parse::<f64>()
        .context("could not parse ffprobe duration")?;

    Ok(AudioProbe {
        sample_rate,
        channels,
        bits_per_sample,
        codec,
        duration_seconds,
    })
}

/// Read a 16-bit PCM WAV from `path` and return mean(|sample|) /
/// i16::MAX in [0.0, 1.0]. Used by the silence-rejection check in
/// `ingest`. Errors if the WAV isn't decodable, isn't 16-bit, or has
/// zero samples.
fn mean_absolute_amplitude(path: &Path) -> Result<f32> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();
    if spec.bits_per_sample != 16 {
        bail!(
            "silence check expects 16-bit PCM, got {} bit on {}",
            spec.bits_per_sample,
            path.display()
        );
    }

    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for sample in reader.samples::<i16>() {
        let s = sample.with_context(|| format!("decoding sample in {}", path.display()))?;
        sum += s.unsigned_abs() as u64;
        count += 1;
    }
    if count == 0 {
        bail!("WAV has zero samples: {}", path.display());
    }
    Ok((sum as f32) / (count as f32) / (i16::MAX as f32))
}

pub fn ingest(input: &Path, output: &Path, cfg: &IngestConfig) -> Result<IngestReport> {
    if !input.exists() {
        bail!("audio source not found: {}", input.display());
    }

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create output dir {}", parent.display()))?;
        }
    }

    // `--` separates ffmpeg's own flags from the positional input/output paths,
    // so a path starting with `-` can never be reinterpreted as a flag.
    //
    // ROADMAP 2.2.1: when `apply_loudnorm` is true (default), we add an
    // EBU R128 loudnorm filter to the chain so all ingested clips land
    // at broadcast level (I=-16 LUFS integrated, TP=-1.5 dBTP true-peak,
    // LRA=11 LU loudness range). Same parameters as scripts/clone_voice.sh
    // so a manual --text speak through a cloned voice matches the level
    // of a pre-rendered pack. Filter chain runs BEFORE resample/channel
    // conversion so the analyzer sees the source's own dynamic range.
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input);
    if cfg.apply_loudnorm {
        cmd.args(["-af", "loudnorm=I=-16:TP=-1.5:LRA=11"]);
    }
    let mut child = cmd
        .args([
            "-ar",
            &cfg.target_sample_rate.to_string(),
            "-ac",
            &cfg.target_channels.to_string(),
            "-acodec",
            "pcm_s16le",
            "-t",
            &format!("{}", cfg.max_seconds + 0.5),
            "--",
        ])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            "failed to spawn ffmpeg (is it installed and on PATH? `brew install ffmpeg`)"
        })?;

    let started = Instant::now();
    let timeout = cfg.ffmpeg_timeout;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "ffmpeg timed out after {:?} on {}",
                        timeout,
                        input.display()
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(e).context("ffmpeg wait failed");
            }
        }
    };

    if !exit_status.success() {
        let mut stderr = String::new();
        if let Some(mut s) = child.stderr.take() {
            use std::io::Read;
            let _ = s.read_to_string(&mut stderr);
        }
        bail!(
            "ffmpeg failed for {} -> {}: {}",
            input.display(),
            output.display(),
            stderr.trim()
        );
    }

    let probed = probe(output)
        .with_context(|| format!("could not probe ffmpeg output {}", output.display()))?;

    if probed.duration_seconds < cfg.min_seconds || probed.duration_seconds > cfg.max_seconds {
        bail!(
            "ingested audio duration {:.2}s outside required range [{:.1}s, {:.1}s] for {}",
            probed.duration_seconds,
            cfg.min_seconds,
            cfg.max_seconds,
            input.display()
        );
    }

    // ROADMAP 2.2.1: silence rejection. Reads the post-encode WAV and
    // computes mean(|sample|) / i16::MAX. Below `silence_min_mean_abs`
    // (default 0.005 ≈ -46 dBFS) means the input was effectively
    // silent — would either crash GPT-SoVITS' Whisper transcription
    // or produce an unusable cloned voice that just outputs hiss.
    // Reject loudly with a clear hint so the user re-records / picks
    // a different clip rather than spending 10 minutes wondering why
    // their clone sounds wrong.
    let mean_abs = mean_absolute_amplitude(output)
        .with_context(|| format!("checking ingested audio level for {}", output.display()))?;
    if mean_abs < cfg.silence_min_mean_abs {
        // Don't leave a silent file behind for the cloning pipeline
        // to choke on later.
        let _ = std::fs::remove_file(output);
        bail!(
            "ingested audio is silent or too quiet (mean |amplitude| = {:.4} of full scale, threshold {:.4}).\n\
             try a louder source: voice clone needs clear speech at conversational volume.\n\
             source: {}",
            mean_abs,
            cfg.silence_min_mean_abs,
            input.display()
        );
    }

    Ok(IngestReport {
        sample_rate: probed.sample_rate,
        channels: probed.channels,
        bits_per_sample: probed.bits_per_sample,
        codec: probed.codec,
        duration_seconds: probed.duration_seconds,
    })
}
