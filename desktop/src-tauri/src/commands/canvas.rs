use tauri::State;

use crate::{
    app_state::AppState,
    events,
    relay::{query_relay, submit_event},
};

/// Reject canvas content larger than [`events::MAX_CONTENT_BYTES`] before it
/// is folded into the DTO returned to the frontend.
///
/// `get_canvas` returns `event.content` verbatim from the relay query, and
/// the relay read path applies no response-size ceiling of its own — only
/// the write path (`events::build_set_canvas`) bounds what a well-behaved
/// client publishes. Without this check, a hostile or nonconforming relay
/// peer can push an arbitrarily large kind:40100 body through this command
/// and into the Files tab's eager `useCanvasQuery` cache.
fn enforce_canvas_content_cap(content: &str) -> Result<(), String> {
    if content.len() > events::MAX_CONTENT_BYTES {
        return Err(format!(
            "canvas content exceeds maximum size of {} bytes (got {})",
            events::MAX_CONTENT_BYTES,
            content.len()
        ));
    }
    Ok(())
}

/// Read the most recent canvas event (kind:40100) for a channel.
#[tauri::command]
pub async fn get_canvas(
    channel_id: String,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let events = query_relay(
        &state,
        &[serde_json::json!({
            "kinds": [40100],
            "#h": [channel_id],
            "limit": 1
        })],
    )
    .await?;

    let Some(event) = events.first() else {
        // Explicit nulls: the TS caller distinguishes "no canvas yet" from
        // "canvas exists" via `updated_at`/`author`, so these keys must be
        // present (absent keys deserialize as `undefined`, not `null`).
        return Ok(serde_json::json!({
            "content": "",
            "event_id": null,
            "updated_at": null,
            "author": null,
        }));
    };

    enforce_canvas_content_cap(&event.content)?;

    Ok(serde_json::json!({
        "content": event.content,
        "event_id": event.id.to_hex(),
        "updated_at": event.created_at.as_secs(),
        "author": event.pubkey.to_hex(),
    }))
}

#[tauri::command]
pub async fn set_canvas(
    channel_id: String,
    content: String,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let uuid = uuid::Uuid::parse_str(&channel_id)
        .map_err(|_| format!("invalid channel UUID: {channel_id}"))?;
    let builder = events::build_set_canvas(uuid, &content)?;
    let result = submit_event(builder, &state).await?;

    Ok(serde_json::json!({
        "ok": true,
        "event_id": result.event_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_at_the_cap_is_accepted() {
        let content = "a".repeat(events::MAX_CONTENT_BYTES);
        assert!(enforce_canvas_content_cap(&content).is_ok());
    }

    #[test]
    fn content_one_byte_over_the_cap_is_rejected() {
        let content = "a".repeat(events::MAX_CONTENT_BYTES + 1);
        assert!(enforce_canvas_content_cap(&content).is_err());
    }
}
