//! Persistent job store. Every prepared ("sliced") job is a directory:
//!
//! ```text
//! <data_dir>/jobs/<id>/
//!   meta.json    - name, settings, size, estimate
//!   egv.bin      - ready-to-send single-pass EGV byte stream
//!   preview.png  - what will be engraved
//!   source.svg   - original SVG (for re-slicing with new settings)
//! ```
//!
//! Printing a stored job never re-slices: the EGV bytes go straight to the
//! device.

use crate::planner::PrepareSettings;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMeta {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub settings: PrepareSettings,
    pub width_mm: f64,
    pub height_mm: f64,
    pub estimate_s: f64,
    pub egv_bytes: u64,
}

pub struct JobStore {
    dir: PathBuf,
    root: PathBuf,
}

impl JobStore {
    pub fn new(data_dir: &Path) -> std::io::Result<JobStore> {
        let dir = data_dir.join("jobs");
        fs::create_dir_all(&dir)?;
        Ok(JobStore { dir, root: data_dir.to_path_buf() })
    }

    ////////////////////////////////////////////////////////////////////////
    // Workspace: the server-side working copy of whatever is on the stage,
    // so the UI survives page reloads and server restarts.
    ////////////////////////////////////////////////////////////////////////

    fn ws_dir(&self) -> PathBuf {
        self.root.join("workspace")
    }

    pub fn workspace_read(&self) -> (Option<String>, Option<serde_json::Value>) {
        let dir = self.ws_dir();
        let svg = fs::read_to_string(dir.join("source.svg")).ok();
        let state = fs::read(dir.join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        (svg, state)
    }

    pub fn workspace_write(
        &self,
        svg: Option<&str>,
        state: &serde_json::Value,
    ) -> std::io::Result<()> {
        let dir = self.ws_dir();
        fs::create_dir_all(&dir)?;
        if let Some(svg) = svg {
            fs::write(dir.join("source.svg"), svg)?;
        }
        fs::write(dir.join("state.json"), serde_json::to_vec_pretty(state)?)?;
        Ok(())
    }

    pub fn workspace_clear(&self) -> std::io::Result<()> {
        let dir = self.ws_dir();
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    ////////////////////////////////////////////////////////////////////////
    // User-editable defaults for the prepare panel.
    ////////////////////////////////////////////////////////////////////////

    pub fn defaults_read(&self) -> serde_json::Value {
        fs::read(self.root.join("defaults.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    pub fn defaults_write(&self, v: &serde_json::Value) -> std::io::Result<()> {
        fs::write(self.root.join("defaults.json"), serde_json::to_vec_pretty(v)?)
    }

    fn job_dir(&self, id: &str) -> Option<PathBuf> {
        // ids are uuids we generated; refuse anything path-like
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        Some(self.dir.join(id))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        name: &str,
        settings: &PrepareSettings,
        svg_source: &str,
        egv: &[u8],
        preview_png: &[u8],
        width_mm: f64,
        height_mm: f64,
        estimate_s: f64,
    ) -> std::io::Result<JobMeta> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.dir.join(&id);
        fs::create_dir_all(&dir)?;
        let meta = JobMeta {
            id: id.clone(),
            name: name.to_string(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            settings: settings.clone(),
            width_mm,
            height_mm,
            estimate_s,
            egv_bytes: egv.len() as u64,
        };
        fs::write(dir.join("egv.bin"), egv)?;
        fs::write(dir.join("preview.png"), preview_png)?;
        fs::write(dir.join("source.svg"), svg_source)?;
        fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        Ok(meta)
    }

    pub fn list(&self) -> Vec<JobMeta> {
        let mut jobs: Vec<JobMeta> = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let meta_path = entry.path().join("meta.json");
                if let Ok(bytes) = fs::read(&meta_path) {
                    if let Ok(meta) = serde_json::from_slice::<JobMeta>(&bytes) {
                        jobs.push(meta);
                    }
                }
            }
        }
        jobs.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        jobs
    }

    pub fn get(&self, id: &str) -> Option<JobMeta> {
        let dir = self.job_dir(id)?;
        let bytes = fs::read(dir.join("meta.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn read_egv(&self, id: &str) -> Option<Vec<u8>> {
        fs::read(self.job_dir(id)?.join("egv.bin")).ok()
    }

    pub fn read_preview(&self, id: &str) -> Option<Vec<u8>> {
        fs::read(self.job_dir(id)?.join("preview.png")).ok()
    }

    pub fn read_source(&self, id: &str) -> Option<String> {
        fs::read_to_string(self.job_dir(id)?.join("source.svg")).ok()
    }

    pub fn rename(&self, id: &str, name: &str) -> Option<JobMeta> {
        let dir = self.job_dir(id)?;
        let mut meta = self.get(id)?;
        meta.name = name.to_string();
        fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta).ok()?).ok()?;
        Some(meta)
    }

    pub fn delete(&self, id: &str) -> bool {
        match self.job_dir(id) {
            Some(dir) if dir.exists() => fs::remove_dir_all(dir).is_ok(),
            _ => false,
        }
    }
}
