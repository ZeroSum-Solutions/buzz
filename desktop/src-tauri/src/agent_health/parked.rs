//! Parked-batch views read from an agent's local park file.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::managed_agents::storage::{load_managed_agents, managed_agent_state_dir};

use super::{agent_may_have_local_state, blocking};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParkedBatchView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub batch_id: String,
    pub channel_id: String,
    pub reason: String,
    pub started: bool,
    pub needs_review: bool,
    pub parked_at: String,
    pub events: usize,
    pub excerpt: String,
}

pub(crate) fn read_parked_batches(dir: &Path) -> Result<Vec<ParkedBatchView>, String> {
    use buzz_acp_pkg::reliability::park::{MAX_PARKED_TOTAL, MAX_PARK_BYTES};

    if !dir.exists() {
        return Ok(Vec::new());
    }
    let park_path = dir.join(buzz_acp_pkg::reliability::park::PARK_FILE);
    if !park_path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&park_path).map_err(|e| format!("open agent park file: {e}"))?;
    use std::io::{BufRead, Read};
    // Bounded exactly like the harness's own park-file reader
    // (`buzz_acp::reliability::park::read_batches`): a total-byte cap via
    // `.take`, and an explicit per-line-length and total-record-count cap —
    // an untrusted park file must not be able to exhaust memory or CPU
    // before a single record is even validated.
    let reader = std::io::BufReader::new(file.take(MAX_PARK_BYTES));
    let mut views = Vec::new();
    for (line_idx, line_res) in reader.lines().enumerate() {
        if views.len() >= MAX_PARKED_TOTAL {
            return Err(format!(
                "park file holds more than {MAX_PARKED_TOTAL} batches"
            ));
        }
        let line = line_res.map_err(|e| format!("read park file line {line_idx}: {e}"))?;
        if line.len() > buzz_acp_pkg::reliability::park::MAX_LINE_BYTES {
            return Err(format!(
                "park file line {line_idx} exceeds the line size cap"
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let batch: buzz_acp_pkg::reliability::park::ParkedBatch = serde_json::from_str(trimmed)
            .map_err(|e| format!("parse park file line {line_idx}: {e}"))?;
        views.push(ParkedBatchView {
            agent: None,
            batch_id: batch.batch_id.to_string(),
            channel_id: batch.channel_id.to_string(),
            reason: batch.reason.as_str().to_string(),
            started: batch.started,
            needs_review: batch.needs_review,
            parked_at: batch.parked_at.to_rfc3339(),
            events: batch.events.len(),
            excerpt: batch
                .events
                .first()
                .map(|e| e.excerpt())
                .unwrap_or_default(),
        });
    }
    Ok(views)
}

#[tauri::command]
pub(crate) async fn get_parked_batches(
    agent: String,
    app: AppHandle,
) -> Result<Vec<ParkedBatchView>, String> {
    blocking::run(move |_proof| {
        let known = load_managed_agents(&app)?;
        if !agent_may_have_local_state(&known, &agent) {
            return Err(format!("agent {agent} is not a locally managed agent"));
        }
        let dir = managed_agent_state_dir(&app, &agent)?;
        let mut batches = read_parked_batches(&dir)?;
        for b in &mut batches {
            b.agent = Some(agent.clone());
        }
        Ok(batches)
    })
    .await
}
