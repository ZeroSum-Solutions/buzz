use std::path::Path;
use std::time::Duration;

use buzz_core::observer::{
    decrypt_observer_payload, encrypt_observer_payload, OBSERVER_FRAME_CONTROL,
};
use nostr::{Event, Keys, PublicKey};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{client::BuzzClient, error::CliError};

pub fn cmd_parked(
    state_root: Option<&Path>,
    agent: Option<&str>,
    json_output: bool,
) -> Result<(), CliError> {
    let rows = parked_rows(state_root, agent)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&rows).map_err(|e| CliError::Other(e.to_string()))?
        );
    } else {
        println!("AGENT\tBATCH\tREASON\tEVENTS\tNEEDS REVIEW");
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                row["agent"].as_str().unwrap_or_default(),
                row["batchId"].as_str().unwrap_or_default(),
                row["reason"].as_str().unwrap_or_default(),
                row["events"],
                row["needsReview"]
            );
        }
    }
    Ok(())
}

fn parked_rows(state_root: Option<&Path>, agent: Option<&str>) -> Result<Vec<Value>, CliError> {
    if let Some(agent) = agent {
        crate::validate::validate_hex64(agent)?;
    }
    let mut rows = Vec::new();
    for directory in super::agents::discover_state_dirs(state_root)? {
        let Some(identity) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if agent.is_some_and(|selected| selected != identity) {
            continue;
        }
        for batch in buzz_acp::reliability::park::read_snapshot(&directory).map_err(|error| {
            CliError::Other(format!(
                "cannot read parked batches for {identity}: {error}"
            ))
        })? {
            if rows.len() >= 10_000 {
                return Err(CliError::Other(
                    "parked listing exceeds 10000 batches; select --agent".into(),
                ));
            }
            rows.push(json!({"agent": identity, "batchId": batch.batch_id, "channelId": batch.channel_id,
                "reason": batch.reason.as_str(), "parkedAt": batch.parked_at, "started": batch.started,
                "needsReview": batch.needs_review, "noticePending": batch.notice_pending, "events": batch.events.len()}));
        }
    }
    Ok(rows)
}

pub async fn control(
    client: &BuzzClient,
    agent: &str,
    batch: Uuid,
    replay: bool,
) -> Result<(), CliError> {
    crate::validate::validate_hex64(agent)?;
    let target = PublicKey::from_hex(agent).map_err(|error| CliError::Other(error.to_string()))?;
    if client
        .auth_tag_owner_hex()
        .is_some_and(|owner| owner != client.keys().public_key().to_hex())
    {
        return Err(CliError::Auth(
            "parked-batch controls require the human owner's signing key".into(),
        ));
    }
    let request = Uuid::new_v4();
    let command = if replay {
        "replay_batch"
    } else {
        "discard_batch"
    };
    let encrypted = encrypt_observer_payload(
        client.keys(),
        &target,
        &json!({"type": command, "batchId": batch, "requestId": request}),
    )
    .map_err(|error| CliError::Other(error.to_string()))?;
    let event =
        buzz_sdk::build_agent_observer_frame(agent, agent, OBSERVER_FRAME_CONTROL, &encrypted)
            .map_err(|error| CliError::Other(error.to_string()))?
            .sign_with_keys(client.keys())
            .map_err(|error| CliError::Other(error.to_string()))?;
    let operation = async {
        let url = client
            .relay_url()
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        let mut connection =
            buzz_ws_client::NostrWsConnection::connect_authenticated(&url, client.keys(), None)
                .await
                .map_err(|error| CliError::Other(error.to_string()))?;
        let subscription = request.to_string();
        connection
            .send_raw(
                &json!(["REQ", subscription, {"kinds": [24200], "authors": [agent],
            "#p": [client.keys().public_key().to_hex()], "#frame": ["telemetry"]}]),
            )
            .await
            .map_err(|error| CliError::Other(error.to_string()))?;
        let accepted = connection
            .send_event(event)
            .await
            .map_err(|error| CliError::Other(error.to_string()))?;
        if !accepted.accepted {
            return Err(CliError::Other(format!(
                "relay refused control: {}",
                accepted.message
            )));
        }
        loop {
            match connection
                .next_event(Duration::from_secs(30))
                .await
                .map_err(|error| CliError::Other(format!("control outcome unconfirmed: {error}")))?
            {
                buzz_ws_client::RelayMessage::Event {
                    subscription_id,
                    event,
                } if subscription_id == subscription => {
                    if let Some(result) =
                        matching_ack(client.keys(), target, &event, request, batch, command)
                    {
                        let result = result?;
                        println!("{result}");
                        return Ok(());
                    }
                }
                buzz_ws_client::RelayMessage::Closed {
                    subscription_id,
                    message,
                } if subscription_id == subscription => {
                    return Err(CliError::Other(format!(
                        "control subscription closed; outcome unconfirmed: {message}"
                    )))
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(60), operation)
        .await
        .map_err(|_| {
            CliError::Other(
                "control timed out; outcome unconfirmed. Inspect parked state before retrying"
                    .into(),
            )
        })?
}

fn matching_ack(
    keys: &Keys,
    agent: PublicKey,
    event: &Event,
    request: Uuid,
    batch: Uuid,
    command: &str,
) -> Option<Result<Value, CliError>> {
    if event.pubkey != agent
        || event.kind.as_u16() != 24200
        || event.verify().is_err()
        || (event.created_at.as_secs() as i128 - chrono::Utc::now().timestamp() as i128).abs() > 300
    {
        return None;
    }
    let has_tag = |name: &str, value: &str| {
        event.tags.iter().any(|tag| {
            let values = tag.as_slice();
            values.first().is_some_and(|v| v == name) && values.get(1).is_some_and(|v| v == value)
        })
    };
    if !has_tag("p", &keys.public_key().to_hex())
        || !has_tag("agent", &agent.to_hex())
        || !has_tag("frame", "telemetry")
    {
        return None;
    }
    let decoded: Value = decrypt_observer_payload(keys, event).ok()?;
    let frames = decoded
        .get("payload")
        .and_then(|payload| payload.get("events"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(&decoded));
    for frame in frames {
        if frame.get("kind").and_then(Value::as_str) != Some("control_result") {
            continue;
        }
        let Some(payload) = frame.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some(command)
            || payload
                .get("requestId")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
                != Some(request)
            || payload
                .get("requestedBatchId")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
                != Some(batch)
        {
            continue;
        }
        let status = payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("missing_status");
        return Some(
            if (command == "replay_batch" && status == "scheduled")
                || (command == "discard_batch" && status == "discarded")
            {
                Ok(payload.clone())
            } else {
                Err(CliError::Other(format!(
                    "harness rejected {command}: {status}"
                )))
            },
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(keys: &Keys, owner: &Keys, request: Uuid, batch: Uuid, status: &str) -> Event {
        let content = encrypt_observer_payload(keys, &owner.public_key(), &json!({"kind": "control_result", "payload": {
            "type": "replay_batch", "requestId": request, "requestedBatchId": batch, "status": status
        }})).unwrap();
        buzz_sdk::build_agent_observer_frame(
            &owner.public_key().to_hex(),
            &keys.public_key().to_hex(),
            "telemetry",
            &content,
        )
        .unwrap()
        .sign_with_keys(keys)
        .unwrap()
    }

    #[test]
    fn parked_control_requires_correlated_agent_ack() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let request = Uuid::new_v4();
        let batch = Uuid::new_v4();
        let event = frame(&agent, &owner, request, batch, "scheduled");
        assert!(matching_ack(
            &owner,
            agent.public_key(),
            &event,
            request,
            batch,
            "replay_batch"
        )
        .unwrap()
        .is_ok());
        assert!(matching_ack(
            &owner,
            agent.public_key(),
            &event,
            Uuid::new_v4(),
            batch,
            "replay_batch"
        )
        .is_none());
        assert!(matching_ack(
            &owner,
            Keys::generate().public_key(),
            &event,
            request,
            batch,
            "replay_batch"
        )
        .is_none());
        assert!(matching_ack(
            &owner,
            agent.public_key(),
            &event,
            request,
            Uuid::new_v4(),
            "replay_batch"
        )
        .is_none());
    }

    #[test]
    fn parked_listing_preserves_corrupt_source_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let identity = "a".repeat(64);
        let directory = root.path().join(&identity);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(buzz_acp::reliability::park::PARK_FILE);
        std::fs::write(&path, b"corrupt source").unwrap();
        assert!(parked_rows(Some(&directory), None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt source");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
    }

    #[test]
    fn parked_control_accepts_correlated_batched_ack() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let request = Uuid::new_v4();
        let batch = Uuid::new_v4();
        let content = encrypt_observer_payload(&agent, &owner.public_key(), &json!({"kind": "batch", "payload": {"events": [
            {"kind": "control_result", "payload": {"type": "discard_batch", "requestId": request, "requestedBatchId": batch, "status": "discarded"}}
        ]}})).unwrap();
        let event = buzz_sdk::build_agent_observer_frame(
            &owner.public_key().to_hex(),
            &agent.public_key().to_hex(),
            "telemetry",
            &content,
        )
        .unwrap()
        .sign_with_keys(&agent)
        .unwrap();
        assert!(matching_ack(
            &owner,
            agent.public_key(),
            &event,
            request,
            batch,
            "discard_batch"
        )
        .unwrap()
        .is_ok());
    }

    #[tokio::test]
    async fn parked_control_roundtrip_subscribes_before_signed_request() {
        use axum::{
            extract::ws::{Message, WebSocketUpgrade},
            routing::get,
            Router,
        };
        async fn receive(socket: &mut axum::extract::ws::WebSocket) -> Value {
            let message = socket.recv().await.unwrap().unwrap();
            serde_json::from_str(message.to_text().unwrap()).unwrap()
        }
        let owner = Keys::generate();
        let agent = Keys::generate();
        let agent_for_server = agent.clone();
        let owner_for_server = owner.clone();
        let batch = Uuid::new_v4();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(move |upgrade: WebSocketUpgrade| {
                let agent = agent_for_server.clone();
                let owner = owner_for_server.clone();
                async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        socket
                            .send(Message::Text(
                                json!(["AUTH", "challenge"]).to_string().into(),
                            ))
                            .await
                            .unwrap();
                        let auth = receive(&mut socket).await;
                        assert_eq!(auth[0], "AUTH");
                        socket
                            .send(Message::Text(
                                json!(["OK", auth[1]["id"], true, ""]).to_string().into(),
                            ))
                            .await
                            .unwrap();
                        let subscription = receive(&mut socket).await;
                        assert_eq!(subscription[0], "REQ");
                        let publish = receive(&mut socket).await;
                        assert_eq!(publish[0], "EVENT");
                        let event: Event = serde_json::from_value(publish[1].clone()).unwrap();
                        assert_eq!(event.pubkey, owner.public_key());
                        assert!(event.verify().is_ok());
                        let payload: Value = decrypt_observer_payload(&agent, &event).unwrap();
                        assert_eq!(payload["batchId"], batch.to_string());
                        let request =
                            Uuid::parse_str(payload["requestId"].as_str().unwrap()).unwrap();
                        socket
                            .send(Message::Text(
                                json!(["OK", event.id.to_hex(), true, ""])
                                    .to_string()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                        for id in [Uuid::new_v4(), request] {
                            let ack = frame(&agent, &owner, id, batch, "scheduled");
                            socket
                                .send(Message::Text(
                                    json!(["EVENT", subscription[1], ack]).to_string().into(),
                                ))
                                .await
                                .unwrap();
                        }
                    })
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = BuzzClient::new(format!("http://{address}"), owner, None, None).unwrap();
        assert!(control(&client, &agent.public_key().to_hex(), batch, true)
            .await
            .is_ok());
        server.abort();
    }

    #[test]
    fn parked_control_rejection_is_not_success() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let request = Uuid::new_v4();
        let batch = Uuid::new_v4();
        let event = frame(&agent, &owner, request, batch, "failed");
        assert!(matching_ack(
            &owner,
            agent.public_key(),
            &event,
            request,
            batch,
            "replay_batch"
        )
        .unwrap()
        .is_err());
    }
}
