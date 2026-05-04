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
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
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

    Ok(IngestReport {
        sample_rate: probed.sample_rate,
        channels: probed.channels,
        bits_per_sample: probed.bits_per_sample,
        codec: probed.codec,
        duration_seconds: probed.duration_seconds,
    })
}
