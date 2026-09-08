//! Alert acknowledgement path: the UI reports which alerts it delivered and
//! the store records them so they are not raised again.

use std::sync::Arc;

use tauri::{AppHandle, State};

use crate::managed_agents::storage::load_managed_agents;
use crate::managed_agents::ManagedAgentRecord;

use super::{
    agent_may_have_local_state, blocking, db_path, open_db, record_alerts, resolve_health_db_scope,
    AgentHealthStore,
};

/// Most acknowledgements one `record_delivered_alerts` call may carry.
/// `evaluate` only ever produces a handful of alerts per sync/frame, so a
/// call this large cannot come from a legitimate delivery batch.
pub(crate) const MAX_ALERTS_PER_ACK: usize = 50;

pub(super) fn validate_alert_ack_count(count: usize) -> Result<(), String> {
    if count > MAX_ALERTS_PER_ACK {
        return Err(format!(
            "record_delivered_alerts accepts at most {MAX_ALERTS_PER_ACK} alerts, got {count}"
        ));
    }
    Ok(())
}

/// Drop any acknowledgement whose `agent` is not a currently locally managed
/// agent, or whose `rule` is not one of the canonical rule identifiers
/// `evaluate` emits.
///
/// `record_delivered_alerts` is a renderer-facing command: nothing ties its
/// `alerts` argument to alerts this backend actually produced and delivered.
/// Without this check, an arbitrary (agent, rule) pair reaches
/// `record_alerts` and suppresses that rule for that agent for the next
/// hour — a real future alert for a real agent silently never fires.
pub(crate) fn filter_valid_alert_acks(
    known: &[ManagedAgentRecord],
    alerts: Vec<crate::agent_health_alerts::Alert>,
) -> Vec<crate::agent_health_alerts::Alert> {
    alerts
        .into_iter()
        .filter(|a| {
            agent_may_have_local_state(known, &a.agent)
                && crate::agent_health_alerts::is_known_rule(&a.rule)
        })
        .collect()
}

#[tauri::command]
pub(crate) async fn record_delivered_alerts(
    alerts: Vec<crate::agent_health_alerts::Alert>,
    app: AppHandle,
    store: State<'_, AgentHealthStore>,
    app_state: State<'_, crate::app_state::AppState>,
) -> Result<(), String> {
    if alerts.is_empty() {
        return Ok(());
    }
    validate_alert_ack_count(alerts.len())?;
    let (relay_url, owner_pubkey) = resolve_health_db_scope(&app_state)?;
    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let known = load_managed_agents(&app)?;
        let valid = filter_valid_alert_acks(&known, alerts);
        if valid.is_empty() {
            return Ok(());
        }
        let conn = open_db(&db_path(&app, &relay_url, &owner_pubkey)?)?;
        let now = chrono::Utc::now().timestamp();
        record_alerts(&conn, &valid, now)
    })
    .await
}
