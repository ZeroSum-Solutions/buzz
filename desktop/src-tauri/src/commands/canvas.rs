use tauri::State;

use crate::{
    app_state::AppState,
    events,
    relay::{query_relay, submit_event},
};

/// Reject canvas content larger than [`events::MAX_CONTENT_BYTES`] before it
/// is folded into the DTO returned to the frontend.
///
/// The relay reader bounds the serialized response before deserialization.
/// This tighter, content-specific cap protects the Files tab cache and matches
/// the write-side canvas limit.
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
    use tauri::Manager;

    #[tokio::test]
    async fn actual_canvas_command_enforces_content_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for size in [events::MAX_CONTENT_BYTES, events::MAX_CONTENT_BYTES + 1] {
            let keys = nostr::Keys::generate();
            let event = nostr::EventBuilder::new(nostr::Kind::Custom(40100), "a".repeat(size))
                .sign_with_keys(&keys)
                .unwrap();
            let body = serde_json::to_string(&vec![event]).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 16_384, "bounded HTTP request headers");
                    request.push(stream.read_u8().await.unwrap());
                }
                // Drain the POST body before closing the socket. Windows can
                // reset a connection with unread request bytes, discarding the
                // response before reqwest consumes it.
                let headers = std::str::from_utf8(&request).unwrap();
                let content_length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(content_length <= 16_384, "bounded HTTP request body");
                let mut request_body = vec![0; content_length];
                stream.read_exact(&mut request_body).await.unwrap();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let state = crate::app_state::build_app_state();
            *state.relay_url_override.lock().unwrap() = Some(format!("ws://{addr}"));
            let app = tauri::test::mock_builder()
                .manage(state)
                .build(tauri::test::mock_context(tauri::test::noop_assets()))
                .unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                get_canvas(uuid::Uuid::new_v4().to_string(), app.state()),
            )
            .await
            .unwrap();
            assert_eq!(
                result.is_ok(),
                size == events::MAX_CONTENT_BYTES,
                "{result:?}"
            );
            if let Ok(value) = result {
                assert_eq!(value["content"].as_str().unwrap().len(), size);
            }
            server.await.unwrap();
        }
    }

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
