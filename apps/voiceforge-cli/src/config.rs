use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePreset {
    pub id: String,
    pub display_name: Option<String>,
    pub reference_wav: Option<String>,
    pub embedding_path: Option<String>,
    pub temperature: Option<f32>,
    pub speed: Option<f32>,
    pub language: Option<String>,
}

pub fn load_presets() -> Result<Vec<VoicePreset>> {
    let path = Path::new("../../configs/presets");
    let mut presets = Vec::new();

    if !path.exists() {
        return Ok(presets);
    }

    for entry in fs::read_dir(path).context("Could not read presets directory")? {
        let entry = entry?;
        let p = entry.path();

        if p.extension().and_then(|s| s.to_str()) == Some("json") {
            let raw = fs::read_to_string(&p)?;
            let preset: VoicePreset = serde_json::from_str(&raw)
                .with_context(|| format!("Invalid preset JSON: {:?}", p))?;
            presets.push(preset);
        }
    }

    Ok(presets)
}
