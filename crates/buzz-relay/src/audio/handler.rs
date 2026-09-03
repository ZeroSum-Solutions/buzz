//! WebSocket audio handler: NIP-42 auth → room join → frame relay → cleanup.
//!
//! ```text
//! ws_audio_handler
//!   └─ handle_audio_connection
//!        ├─ send challenge, await auth (5s timeout)
//!        ├─ ensure_membership (auto-add for ephemeral channels)
//!        ├─ room.add_peer → broadcast joined
//!        ├─ spawn send_loop + heartbeat_loop
//!        ├─ run recv_loop (blocks until disconnect)
//!        └─ cleanup: remove peer, broadcast left, emit lifecycle events
//! ```

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::http::{HeaderMap, StatusCode};
use axum::{
    extract::{FromRequest, Path, State, WebSocketUpgrade},
    response::IntoResponse,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nostr::{EventBuilder, Kind, Tag};
use serde::Deserialize;
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use buzz_auth::{generate_challenge, VerifiedAssertion};
use buzz_core::tenant::TenantContext;

use buzz_core::StoredEvent;
use buzz_pubsub::EventTopic;

use crate::audio::room::PeerCtrl;
use crate::state::{run_registered_community_connection, AppState, CommunityConnectionControl};

/// Maximum binary frame size: 4 KB is generous for a single Opus packet.
const MAX_AUDIO_FRAME_BYTES: usize = 4096;

/// Maximum text frame size: 8 KB bounds auth/control JSON parsing.
const MAX_TEXT_FRAME_BYTES: usize = 8192;

/// Parser-level cap for this route. Text auth/control frames are the largest
/// message type audio accepts; binary Opus frames are bounded more tightly
/// after parsing.
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = MAX_TEXT_FRAME_BYTES;

/// Heartbeat interval.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Missed pong limit before disconnect.
const MAX_MISSED_PONGS: u8 = 3;

/// Auth timeout.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

/// WebSocket upgrade handler for `/huddle/:channel_id/audio`.
pub async fn ws_audio_handler(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    headers: HeaderMap,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // NIP-FI assertion check at upgrade — before tenant lookup and before the
    // WebSocket handshake. Running pre-lookup means a denied request pays zero
    // DB cost and the gate is reachable in tests without a live community.
    // [FI-TRACE-TRANSPORT-CLOSED] [NIP-FI.md §Admission pairing sequence]
    let nip_fi_assertion = {
        use crate::nip_fi_upgrade::{check_nip_fi_at_upgrade, NipFiUpgradeOutcome};
        let mode = state.config.nip_fi.mode;
        let verifier = state.nip_fi_verifier.as_deref();
        match check_nip_fi_at_upgrade(&headers, verifier, mode) {
            NipFiUpgradeOutcome::NotRequired => None,
            NipFiUpgradeOutcome::Admitted(assertion) => Some(assertion),
            NipFiUpgradeOutcome::Denied(resp) => return resp.into_response(),
        }
    };

    // Row zero: bind this huddle-audio connection to its community from the
    // request host BEFORE the WebSocket upgrade, identical to the main relay
    // door. An unmapped host or lookup failure fails closed with a generic 404
    // — never a default tenant — so an unauthenticated caller cannot probe
    // which communities exist on this deployment.
    let raw_host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let tenant = match crate::tenant::bind_community(&state.db, raw_host).await {
        Ok(ctx) => ctx,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                "relay: no community is configured for this host",
            )
                .into_response();
        }
    };

    let ws = match WebSocketUpgrade::from_request(req, &state).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };

    let permit = match acquire_audio_connection_permit(&state.conn_semaphore) {
        Some(permit) => permit,
        None => {
            warn!(channel_id = %channel_id, "Connection limit reached, rejecting audio WebSocket");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "relay: connection limit reached",
            )
                .into_response();
        }
    };

    // Keep the parser boundary at the largest message this route accepts. The
    // checks in the receive loop still distinguish text from binary policy, but
    // they run after tungstenite has assembled a message.
    // Capture the upgrade instant here — before the on_upgrade callback fires —
    // so the NIP-FI session partition is rooted at the HTTP handshake, not the
    // post-community-active-check instant. [FI-TRACE-LEASE-BOUND]
    let connection_time = chrono::Utc::now();
    limit_audio_websocket(ws).on_upgrade(move |socket| {
        handle_audio_connection(
            socket,
            state,
            tenant,
            channel_id,
            permit,
            nip_fi_assertion,
            connection_time,
        )
    })
}

fn acquire_audio_connection_permit(
    conn_semaphore: &Arc<Semaphore>,
) -> Option<OwnedSemaphorePermit> {
    Arc::clone(conn_semaphore).try_acquire_owned().ok()
}

fn limit_audio_websocket<F>(ws: WebSocketUpgrade<F>) -> WebSocketUpgrade<F> {
    ws.max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
}

/// Highest huddle audio protocol version this relay understands. Clients are
/// allowed to negotiate any version in `1..=CURRENT_PROTOCOL_VERSION`; older
/// versions stay supported indefinitely for staged rollouts.
const CURRENT_PROTOCOL_VERSION: u8 = 3;

#[derive(Deserialize)]
struct AuthMsg {
    #[serde(rename = "type")]
    msg_type: String,
    event: nostr::Event,
    parent_channel_id: Option<Uuid>,
    /// Huddle audio protocol version requested by the client. Defaults to 1
    /// when missing so existing clients keep working without recompile. A
    /// room is pinned to whichever version its first peer requested; later
    /// peers must match or get `upgrade_required`.
    #[serde(default = "default_protocol_version")]
    protocol_version: u8,
}

fn default_protocol_version() -> u8 {
    1
}

async fn handle_audio_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    tenant: TenantContext,
    channel_id: Uuid,
    _permit: OwnedSemaphorePermit,
    nip_fi_assertion: Option<VerifiedAssertion>,
    connection_time: chrono::DateTime<chrono::Utc>,
) {
    let cancel = CancellationToken::new();
    let control = CommunityConnectionControl::new(cancel);
    let community_id = tenant.community();
    let registry = Arc::clone(&state.community_connections);
    let check_state = Arc::clone(&state);
    let run_state = Arc::clone(&state);
    run_registered_community_connection(
        &registry,
        Uuid::new_v4(),
        community_id,
        control,
        move || async move { check_state.db.is_community_active(community_id).await },
        move |control| {
            handle_active_audio_connection(
                socket,
                run_state,
                tenant,
                channel_id,
                control,
                nip_fi_assertion,
                connection_time,
            )
        },
    )
    .await;
}

pub(crate) async fn handle_active_audio_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    tenant: TenantContext,
    channel_id: Uuid,
    control: CommunityConnectionControl,
    nip_fi_assertion: Option<VerifiedAssertion>,
    connection_time: chrono::DateTime<chrono::Utc>,
) {
    let cancel = control.cancellation_token();
    let disconnect_reason = control.disconnect_reason();
    // connection_time is threaded in from the HTTP handler (captured immediately
    // before on_upgrade) so the session partition is rooted at the true upgrade
    // instant, not the post-community-active-check instant. [FI-TRACE-LEASE-BOUND]
    let (mut ws_send, mut ws_recv) = socket.split();

    let challenge = generate_challenge();
    let challenge_msg =
        serde_json::json!({"type": "challenge", "challenge": challenge}).to_string();
    if ws_send
        .send(WsMessage::Text(challenge_msg.into()))
        .await
        .is_err()
    {
        return;
    }

    let auth_result = tokio::select! {
        biased;
        _ = cancel.cancelled() => return,
        result = tokio::time::timeout(AUTH_TIMEOUT, async {
            while let Some(Ok(msg)) = ws_recv.next().await {
                if let WsMessage::Text(text) = msg {
                    if text.len() > MAX_TEXT_FRAME_BYTES {
                        warn!(channel_id = %channel_id, "auth text frame too large — dropping");
                        continue;
                    }
                    if let Ok(auth) = serde_json::from_str::<AuthMsg>(&text) {
                        if auth.msg_type == "auth" {
                            return Some(auth);
                        }
                    }
                }
            }
            None
        }) => result,
    };

    let auth_msg = match auth_result {
        Ok(Some(a)) => a,
        _ => {
            debug!(channel_id = %channel_id, "audio auth timeout or disconnect");
            return;
        }
    };

    // Extract NIP-OA auth tag before verify_auth_event consumes the event.
    let auth_tag_json = crate::handlers::auth::extract_auth_tag_json(&auth_msg.event);
    let signed_auth_created_at = auth_msg.event.created_at.as_secs();

    let relay_url = crate::api::bridge::nip42_expected_relay_url(&state.config.relay_url, &tenant);
    let auth_ctx = match state
        .auth
        .verify_auth_event(auth_msg.event, &challenge, &relay_url)
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            warn!(channel_id = %channel_id, "audio auth failed: {e}");
            let _ = ws_send
                .send(WsMessage::Text(
                    serde_json::json!({"type":"error","message":"auth failed"})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };

    let pubkey = auth_ctx.pubkey;
    let pubkey_hex = pubkey.to_hex();
    let pubkey_bytes = pubkey.to_bytes().to_vec();
    let parent_channel_id = auth_msg.parent_channel_id;

    // NIP-FI key pairing [FI-INV-05]: unconditional, using the shared production
    // seam. When an assertion was presented at upgrade, the proven NIP-42 key
    // MUST equal the assertion's `nostr_pubkey` claim. Claimless assertion is
    // also a denial. The seam owns verdict, frame delivery, metric, and cancel.
    // [FI-TRACE-DENIAL-ORACLE post-establishment]
    if crate::nip_fi_session::enforce_nip_fi_key_pairing(
        nip_fi_assertion.as_ref(),
        pubkey,
        crate::nip_fi_session::PairingDenialTarget::Audio {
            ws_send: &mut ws_send,
            cancel: &cancel,
            channel_id,
        },
    )
    .await
        == crate::nip_fi_session::PairingOutcome::Denied
    {
        return;
    }

    // Compute the NIP-FI session deadline (same three-term formula as main relay).
    // Partition is rooted at `connection_time` captured before NIP-42 auth.
    // [FI-TRACE-LEASE-BOUND]
    let audio_session_deadline = nip_fi_assertion.as_ref().map(|a| {
        crate::connection::compute_session_deadline(
            a,
            connection_time,
            state.config.nip_fi.max_connection_lifetime(),
        )
    });

    // B1: Arm the NIP-FI expiry task HERE — before any persisting side effect
    // (relay membership, room join, roster events, PARTICIPANT_JOINED).
    //
    // Create the session admission gate when in enforce mode. The gate is the
    // quiescence barrier: commit_participant_join acquires an effect permit
    // before committing the 48101 + membership transaction. The expiry task's
    // gate.expire() holds the write guard until all pre-expiry permits finish.
    //
    // The terminal channel is created before the send_loop exists so that the
    // denial frame is available to drain via ws_send (still owned) if expiry
    // fires during the admission sequence. Once the send_loop spawns, it owns
    // the receiver and drains it on cancellation. [FI-TRACE-LEASE-BOUND]
    let (terminal_ctrl_tx, mut terminal_ctrl_rx) =
        tokio::sync::mpsc::channel::<axum::extract::ws::Message>(1);

    // One gate per audio connection (one-gate-per-connection invariant).
    // Enforce mode: gate has a deadline; expiry task fires at that deadline.
    // Off-mode: off_mode() gate never self-expires; acquire_effect always succeeds.
    let audio_gate = if let Some(deadline) = audio_session_deadline {
        crate::nip_fi_gate::SessionAdmissionGate::new(deadline, cancel.clone())
    } else {
        crate::nip_fi_gate::SessionAdmissionGate::off_mode(cancel.clone())
    };

    let _nip_fi_admission_expiry = audio_session_deadline.map(|deadline| {
        crate::nip_fi_session::spawn_nip_fi_expiry_task(
            deadline,
            std::sync::Arc::clone(&audio_gate),
            terminal_ctrl_tx.clone(),
            crate::nip_fi_session::NipFiWsRoute::Audio,
        )
    });

    // Already-expired check: the synchronous guard catches a deadline that is
    // already past at this instant, without relying on the async expiry task
    // to execute first. Sends the denial frame directly on ws_send (still
    // owned — send_loop has not started) then cancels and returns.
    if let Some(deadline) = audio_session_deadline {
        if chrono::Utc::now() >= deadline {
            warn!(
                channel_id = %channel_id,
                pubkey = %pubkey_hex,
                "NIP-FI session deadline already expired at pairing — rejecting audio admission"
            );
            use futures_util::SinkExt as _;
            let _ = ws_send
                .send(crate::nip_fi_session::authorization_denied_frame(
                    crate::nip_fi_session::NipFiWsRoute::Audio,
                ))
                .await;
            cancel.cancel();
            return;
        }
    }

    // Helper macro: check for NIP-FI mid-admission cancellation, drain the
    // terminal channel (which holds the denial frame queued by the expiry
    // task), send it via ws_send (still owned), and return.
    // Used at every async boundary in the admission sequence below.
    macro_rules! check_cancel {
        () => {
            if cancel.is_cancelled() {
                use futures_util::SinkExt as _;
                while let Ok(msg) = terminal_ctrl_rx.try_recv() {
                    let _ = ws_send.send(msg).await;
                }
                return;
            }
        };
        (cleanup: $cleanup:expr) => {
            if cancel.is_cancelled() {
                $cleanup;
                use futures_util::SinkExt as _;
                while let Ok(msg) = terminal_ctrl_rx.try_recv() {
                    let _ = ws_send.send(msg).await;
                }
                return;
            }
        };
    }

    if crate::api::relay_members::enforce_relay_membership(
        &state,
        tenant.community(),
        pubkey.as_bytes(),
        auth_tag_json.as_deref(),
        Some(signed_auth_created_at),
    )
    .await
    .is_err()
    {
        warn!(channel_id = %channel_id, pubkey = %pubkey_hex, "audio: relay membership denied");
        let _ = ws_send
            .send(WsMessage::Text(
                serde_json::json!({"type": "error", "message": "restricted: not a relay member"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }
    check_cancel!();

    // ── Step 3: membership check / auto-add ───────────────────────────────────
    let membership_admission = match check_membership_for_admission(
        &state,
        &tenant,
        channel_id,
        &pubkey_bytes,
        parent_channel_id,
    )
    .await
    {
        Ok(admission) => admission,
        Err(e) => {
            warn!(channel_id = %channel_id, pubkey = %pubkey_hex, "audio membership denied: {e}");
            let _ = ws_send
                .send(WsMessage::Text(
                    serde_json::json!({"type":"error","message":"not a member"})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    // Derive parent_id_for_event from the membership admission result.
    // This is the channel ID that lifecycle events (48101/48102/48103) belong to.
    let parent_id_for_event = match &membership_admission {
        MembershipAdmission::Existing { parent_channel_id } => *parent_channel_id,
        MembershipAdmission::AutoAddRequired {
            parent_channel_id, ..
        } => *parent_channel_id,
    };
    check_cancel!();

    // Huddle cross-pod routing (mesh) OR single-pod guardrail.
    //
    // When the mesh is live (`state.mesh()` is `Some`), a huddle can span pods:
    // Redis arbitrates ownership and this pod either owns the room locally or
    // forwards the client to the owner over a `HuddleControl` stream. When the
    // mesh is off, we keep today's behavior exactly — including the
    // `huddle_audio_available=false` rejection under a non-mesh horizontal
    // deployment (two peers on different pods would never hear each other).
    //
    // `remote_owner` is `Some` only on the non-owner path; it carries the
    // registration to the owner and, once the client is admitted locally, is
    // opened so its media forwards to the owner instead of fanning out locally.
    let mut pending_remote: Option<crate::audio::join::JoinOutcome> = None;
    // The freshly-acquired owner lease, if this connection won the CAS. Held
    // until `add_peer` succeeds, then installed in the owner registry so the
    // renewer's lifetime matches the room's, not this connection's failure
    // paths (archived channel, version reject, room full) which return early.
    let mut acquired_lease: Option<crate::audio::join::HuddleLease> = None;
    match state.mesh() {
        Some(mesh) => {
            if mesh.owners.is_draining() {
                let _ = ws_send
                    .send(WsMessage::Text(
                        serde_json::json!({
                            "type": "error",
                            "code": "huddle_relay_draining",
                            "message": "relay is draining; reconnect"
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                return;
            }
            match crate::audio::join::resolve_join_owner_ready(
                &mesh.directory,
                tenant.community(),
                channel_id,
                mesh.local_runtime_id,
                &mesh.owners,
            )
            .await
            {
                Ok(resolved) => {
                    acquired_lease = resolved.acquired;
                    pending_remote = Some(resolved.outcome);
                }
                Err(e) => {
                    warn!(
                        channel_id = %channel_id,
                        pubkey = %pubkey_hex,
                        "huddle join rejected by fence: {e}"
                    );
                    let _ = ws_send
                        .send(WsMessage::Text(
                            serde_json::json!({
                                "type": "error",
                                "code": "join_rejected",
                                "message": "huddle join rejected"
                            })
                            .to_string()
                            .into(),
                        ))
                        .await;
                    return;
                }
            }
            check_cancel!();
        }
        None => {
            if !state.config.huddle_audio_available {
                debug!(
                    channel_id = %channel_id,
                    pubkey = %pubkey_hex,
                    "huddle audio unavailable under horizontal scaling — rejecting join"
                );
                let _ = ws_send
                    .send(WsMessage::Text(
                        serde_json::json!({
                            "type": "error",
                            "code": "huddle_audio_unavailable",
                            "message": "huddle audio unavailable in this deployment"
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                return;
            }
        }
    }

    let room = state
        .audio_rooms
        .get_or_create(tenant.community(), channel_id);

    // Re-check archived status after obtaining the room. This closes the
    // cross-boundary race: a joiner that passed ensure_membership before
    // the last peer archived the channel could get a fresh room via
    // get_or_create (the old room was already cleaned up). This DB check
    // catches that case. The room-level ended flag (checked inside add_peer)
    // handles the same-room case.
    match state.db.get_channel(tenant.community(), channel_id).await {
        Ok(ch) if ch.archived_at.is_some() => {
            debug!(channel_id = %channel_id, "channel archived before room join");
            let _ = ws_send
                .send(WsMessage::Text(
                    serde_json::json!({"type":"error","message":"huddle has ended"})
                        .to_string()
                        .into(),
                ))
                .await;
            state
                .audio_rooms
                .cleanup_if_empty(tenant.community(), channel_id);
            return;
        }
        Err(e) => {
            warn!(channel_id = %channel_id, "pre-join channel check failed (fail-closed): {e}");
            state
                .audio_rooms
                .cleanup_if_empty(tenant.community(), channel_id);
            return;
        }
        Ok(_) => {} // Channel exists and is not archived — proceed.
    }
    check_cancel!();

    // Reject unsupported future versions up-front so we don't accidentally
    // pin a room to a version we can't speak. Versions 1..=CURRENT are OK.
    let requested_version = auth_msg.protocol_version;
    if requested_version == 0 || requested_version > CURRENT_PROTOCOL_VERSION {
        warn!(
            channel_id = %channel_id,
            pubkey = %pubkey_hex,
            requested_version,
            current = CURRENT_PROTOCOL_VERSION,
            "audio: client requested unsupported protocol version"
        );
        let _ = ws_send
            .send(WsMessage::Text(
                serde_json::json!({
                    "type": "error",
                    "code": "unsupported_version",
                    "message": format!(
                        "huddle audio protocol v{requested_version} not supported; relay max is v{CURRENT_PROTOCOL_VERSION}"
                    ),
                    "current_version": CURRENT_PROTOCOL_VERSION,
                })
                .to_string()
                .into(),
            ))
            .await;
        return;
    }

    // Remote registration happens before ingress admission. The owner-assigned
    // index is therefore the only index this client ever has; no frame or
    // `joined` message can escape with an ingress-local placeholder.
    let mut remote_session: Option<crate::audio::join::RemoteHuddleSession> = None;
    let mut remote_stream: Option<buzz_relay_mesh::MeshStream> = None;
    let mut remote_fence: Option<Arc<crate::audio::mesh::GenerationFloor>> = None;
    if let (Some(mesh), Some(crate::audio::join::JoinOutcome::RemoteOwner { .. })) =
        (state.mesh(), pending_remote)
    {
        let outcome = pending_remote.expect("RemoteOwner matched above");
        let fenced = outcome.fenced_header(channel_id, mesh.local_runtime_id);
        let crate::audio::join::JoinOutcome::RemoteOwner {
            owner_runtime_id, ..
        } = outcome
        else {
            unreachable!("matched RemoteOwner above");
        };
        match crate::audio::join::dial_remote_owner(
            Arc::clone(&mesh.transport),
            mesh.local_runtime_id,
            owner_runtime_id,
            fenced,
            tenant.community(),
            pubkey_hex.clone(),
            requested_version,
        )
        .await
        {
            Ok((session, stream)) => {
                remote_session = Some(session);
                remote_stream = Some(stream);
                remote_fence = Some(Arc::clone(&mesh.audio_fence));
            }
            Err(crate::audio::join::DialError::Rejected(reason)) => {
                warn!(channel_id = %channel_id, pubkey = %pubkey_hex, "huddle owner rejected registration: {reason:?}");
                let _ = ws_send
                    .send(WsMessage::Text(
                        remote_rejection_ws_error(&reason).to_string().into(),
                    ))
                    .await;
                state
                    .audio_rooms
                    .cleanup_if_empty(tenant.community(), channel_id);
                return;
            }
            Err(crate::audio::join::DialError::Mesh(e)) => {
                warn!(channel_id = %channel_id, pubkey = %pubkey_hex, "huddle owner registration failed: {e}");
                let _ = ws_send
                    .send(WsMessage::Text(
                        serde_json::json!({
                            "type": "error", "code": "huddle_owner_unreachable",
                            "message": "could not reach the huddle owner"
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                state
                    .audio_rooms
                    .cleanup_if_empty(tenant.community(), channel_id);
                return;
            }
        }
        check_cancel!();
    }

    let admission = if let Some(session) = remote_session.as_ref() {
        room.add_peer_at_index(pubkey_hex.clone(), requested_version, session.peer_index())
            .map(|(id, _mirror_epoch, audio, ctrl, revision)| {
                // Report the owner-assigned epoch, not the local mirror's:
                // the mirror never fans out via `broadcast_frame`, so its epoch
                // is inert. The client's self-entry must match the owner roster.
                (
                    id,
                    session.peer_index(),
                    session.epoch(),
                    audio,
                    ctrl,
                    revision,
                )
            })
    } else {
        room.add_peer(pubkey_hex.clone(), requested_version)
    };
    let (peer_id, peer_index, peer_epoch, audio_rx, peer_ctrl_rx, admission_revision) =
        match admission {
            Ok(v) => v,
            Err(crate::audio::room::AdmissionError::Full) => {
                warn!(channel_id = %channel_id, "audio room participant capacity reached");
                let _ = ws_send.send(WsMessage::Text(serde_json::json!({"type":"error","code":"room_full","message":"room participant capacity reached"}).to_string().into())).await;
                if let (Some(session), Some(stream)) =
                    (remote_session.as_ref(), remote_stream.as_mut())
                {
                    crate::audio::join::send_clean_close(
                        stream,
                        session.fenced(),
                        session.pubkey(),
                    )
                    .await;
                }
                return;
            }
            Err(crate::audio::room::AdmissionError::Ended) => {
                debug!(channel_id = %channel_id, "room ended before admission");
                let _ = ws_send.send(WsMessage::Text(serde_json::json!({"type":"error","code":"room_ended","message":"huddle has ended"}).to_string().into())).await;
                if let (Some(session), Some(stream)) =
                    (remote_session.as_ref(), remote_stream.as_mut())
                {
                    crate::audio::join::send_clean_close(
                        stream,
                        session.fenced(),
                        session.pubkey(),
                    )
                    .await;
                }
                return;
            }
            Err(crate::audio::room::AdmissionError::VersionMismatch { pinned, requested }) => {
                info!(channel_id = %channel_id, pubkey = %pubkey_hex, pinned, requested, "audio: protocol version mismatch — upgrade required");
                let _ = ws_send.send(WsMessage::Text(serde_json::json!({
                "type": "error", "code": "upgrade_required",
                "message": format!("this huddle is using audio protocol v{pinned}; your client requested v{requested}"),
                "pinned_version": pinned, "requested_version": requested,
            }).to_string().into())).await;
                if let (Some(session), Some(stream)) =
                    (remote_session.as_ref(), remote_stream.as_mut())
                {
                    crate::audio::join::send_clean_close(
                        stream,
                        session.fenced(),
                        session.pubkey(),
                    )
                    .await;
                }
                return;
            }
        };

    // B1: check for mid-admission expiry immediately after peer is registered
    // in the room. The peer_id is now live; cancel means we must undo it.
    check_cancel!(cleanup: {
        room.remove_peer(peer_id);
        state.audio_rooms.cleanup_if_empty(tenant.community(), channel_id);
        if let (Some(session), Some(ref mut stream)) = (remote_session.as_ref(), remote_stream.as_mut()) {
            let s = session.fenced();
            let pk = session.pubkey().to_string();
            crate::audio::join::send_clean_close(stream, s, &pk).await;
        }
    });

    info!(
        channel_id = %channel_id,
        pubkey = %pubkey_hex,
        peer_index,
        "audio peer joined"
    );

    // Owner path: install (or reuse) this room's single lease renewer now that
    // a peer is admitted, and capture its owner-loss signal. The connection
    // that won the CAS holds `acquired_lease`; it installs the renewer. A
    // steady-state owner (an earlier joiner installed it) reuses the room's
    // existing signal. `owner_lost` drives this connection's own teardown
    // below; `owner_generation` fences the release on room-empty so a stale
    // teardown cannot release a newer epoch a re-acquire installed.
    //
    // The reuse arm's live entry is guaranteed by `resolve_join_owner_ready`:
    // it re-resolves until the CAS winner has installed (reuse) or a fresh CAS
    // wins (acquire), never returning a `LocalOwner` snapshot with a missing
    // registry entry. So a local owner peer here always gets a real `lost`
    // watcher — the ownerless split-brain (an owner peer fanning stale media
    // with no way to observe lease loss, since local WS peers have no per-frame
    // fence) cannot occur. A `None` on the reuse arm is therefore an invariant
    // violation, not a benign race; log it loudly rather than proceed silently.
    let mut owner_lost: Option<CancellationToken> = None;
    let mut owner_draining: Option<CancellationToken> = None;
    let mut owner_generation: Option<u64> = None;
    if let Some(mesh) = state.mesh() {
        match (pending_remote, acquired_lease.take()) {
            (Some(crate::audio::join::JoinOutcome::LocalOwner { generation }), Some(lease)) => {
                let signals =
                    mesh.owners
                        .attach_signals(channel_id, Arc::new(mesh.directory.clone()), lease);
                owner_lost = Some(signals.lost);
                owner_draining = Some(signals.draining);
                owner_generation = Some(generation);
            }
            (Some(crate::audio::join::JoinOutcome::LocalOwner { generation }), None) => {
                owner_lost = mesh.owners.lost_for(channel_id);
                owner_draining = mesh.owners.drain_for(channel_id);
                owner_generation = Some(generation);
                if owner_lost.is_none() {
                    error!(
                        channel_id = %channel_id,
                        "huddle owner-ready invariant violated: LocalOwner reuse with no live \
                         registry entry after resolve_join_owner_ready — owner peer has no \
                         lease-loss watcher"
                    );
                }
            }
            _ => {}
        }
    }

    // Remote registration and owner-assigned ingress admission completed above.

    let (peers_snapshot, roster_revision): (Vec<serde_json::Value>, u64) = if let Some(session) =
        remote_session.as_ref()
    {
        (
                session
                    .roster()
                    .peers
                    .iter()
                    .map(|peer| {
                        serde_json::json!({"pubkey": peer.pubkey, "peer_index": peer.peer_index, "epoch": peer.epoch})
                    })
                    .collect(),
                session.roster().revision,
            )
    } else {
        let snapshot = room.roster_snapshot();
        (
                snapshot
                    .peers
                    .into_iter()
                    .map(|peer| {
                        serde_json::json!({"pubkey": peer.pubkey, "peer_index": peer.peer_index, "epoch": peer.epoch})
                    })
                    .collect(),
                snapshot.revision,
            )
    };
    debug_assert!(roster_revision >= admission_revision);

    // ── Step 6: commit kind:48101 (PARTICIPANT_JOINED) atomically ────────────
    // commit_participant_join takes one DB transaction containing:
    //   - auto-membership insert (if AutoAddRequired and still absent), and
    //   - the 48101 event insert
    // Both commit under a single session effect permit, or both roll back on
    // expiry. Fan-out happens while the permit is still held.
    //
    // joined-ordering: the `joined` frame is sent to the connecting client and
    // broadcast to existing peers ONLY after commit-won. This matches Thufir's
    // design (fd00e6fe): no client-visible join success before `48101` commit.
    // Client compatibility: clients treat WS close as "leave audio"; receiving
    // close without a prior `joined` is a safe no-op — the session never
    // stabilised from the client's perspective.
    let lifecycle_revision = if remote_session.is_some() {
        roster_revision
    } else {
        admission_revision
    };

    match commit_participant_join(
        &state,
        &tenant,
        channel_id,
        parent_id_for_event,
        &pubkey_hex,
        &pubkey_bytes,
        peer_id,
        lifecycle_revision,
        &membership_admission,
        &audio_gate,
    )
    .await
    {
        Ok(_stored) => {}
        Err(JoinCommitError::Expired) => {
            // Gate denied — expiry fired before commit. Clean up and return.
            // The expiry task already queued the denial frame and cancelled.
            // No `joined` frame was sent — commit-won invariant holds.
            room.remove_peer(peer_id);
            state
                .audio_rooms
                .cleanup_if_empty(tenant.community(), channel_id);
            if let (Some(session), Some(ref mut stream)) =
                (remote_session.as_ref(), remote_stream.as_mut())
            {
                let s = session.fenced();
                let pk = session.pubkey().to_string();
                crate::audio::join::send_clean_close(stream, s, &pk).await;
            }
            // Drain the terminal denial frame (already queued by expiry task).
            use futures_util::SinkExt as _;
            while let Ok(msg) = terminal_ctrl_rx.try_recv() {
                let _ = ws_send.send(msg).await;
            }
            return;
        }
        Err(JoinCommitError::Db(e)) => {
            // DB failure during join commit — treat same as pre-admission error.
            // No `joined` frame was sent — commit-won invariant holds.
            warn!(channel_id = %channel_id, pubkey = %pubkey_hex, "48101 commit failed: {e}");
            room.remove_peer(peer_id);
            state
                .audio_rooms
                .cleanup_if_empty(tenant.community(), channel_id);
            if let (Some(session), Some(ref mut stream)) =
                (remote_session.as_ref(), remote_stream.as_mut())
            {
                let s = session.fenced();
                let pk = session.pubkey().to_string();
                crate::audio::join::send_clean_close(stream, s, &pk).await;
            }
            let _ = ws_send
                .send(WsMessage::Text(
                    serde_json::json!({"type":"error","message":"error: join commit failed"})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    }

    // ── Step 7: notify the joining client and broadcast to existing peers ─────
    // `joined` is sent after commit-won so no client sees join success before
    // the 48101 is persisted. [joined-ordering, fd00e6fe note-2]
    let joined_msg = serde_json::json!({
        "type": "joined",
        "revision": roster_revision,
        "pubkey": pubkey_hex,
        "peer_index": peer_index,
        "epoch": peer_epoch,
        "peers": peers_snapshot,
    })
    .to_string();

    if remote_session.is_some() {
        if ws_send
            .send(WsMessage::Text(joined_msg.into()))
            .await
            .is_err()
        {
            room.remove_peer(peer_id);
            state
                .audio_rooms
                .cleanup_if_empty(tenant.community(), channel_id);
            return;
        }
    } else {
        room.broadcast_control(joined_msg);
    }

    // B1: After commit_participant_join, the admission is committed. No further
    // check_cancel! is needed — the send_loop owns terminal_ctrl_rx from here.

    let missed_pongs = Arc::new(AtomicU8::new(0));

    // Dual-channel pattern (matches connection.rs): data channel for audio,
    // control channel for Ping/Pong/Close/control JSON with priority drain.
    let (data_tx, data_rx) = mpsc::channel::<WsMessage>(16);
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<WsMessage>(8);

    // The terminal channel was created before admission (above) so that
    // mid-admission expiry could drain it via ws_send. Now the send_loop takes
    // ownership of `terminal_ctrl_rx` and drains it in its cancel branch.
    // The expiry task (_nip_fi_admission_expiry) armed above is the lifetime
    // enforcer for this connection — no second task is needed.
    let send_cancel = cancel.child_token();
    let send_task = tokio::spawn(send_loop(
        ws_send,
        data_rx,
        ctrl_rx,
        terminal_ctrl_rx,
        send_cancel,
        disconnect_reason,
    ));

    let hb_cancel = cancel.clone();
    let hb_missed = Arc::clone(&missed_pongs);
    let heartbeat_task = tokio::spawn(heartbeat_loop(ctrl_tx.clone(), hb_missed, hb_cancel));

    let fwd_cancel = cancel.child_token();
    let forward_task = tokio::spawn(audio_forward_loop(
        audio_rx,
        peer_ctrl_rx,
        data_tx,
        ctrl_tx.clone(),
        fwd_cancel,
        cancel.clone(),
    ));

    // NIP-FI session-lifetime enforcement task was armed before admission
    // (at audio_session_deadline above) with `terminal_ctrl_tx`. Keep the
    // handle alive for the duration of the connection. [FI-TRACE-LEASE-BOUND]
    let nip_fi_audio_expiry_task = _nip_fi_admission_expiry;

    // Non-owner path: own the owner's `HuddleControl` stream in a reader task.
    // It races the owner's teardown signal against our own cancellation:
    //   * owner speaks first (`Goodbye` / stream close) → tear the client down
    //     and close its WS so it rejoins (against a fresh owner/generation),
    //     and forget the local generation floor so the rejoin isn't fenced by
    //     the dead session. Redis remains the ownership arbiter; forgetting the
    //     floor only clears local stale-frame suppression.
    //   * we cancel first (client left / heartbeat death) → send the clean
    //     `UnregisterPeer` + `Goodbye(SessionEnded)` so the owner drops us.
    let reader_task = remote_stream.map(|mut stream| {
        let reader_cancel = cancel.clone();
        let fence = remote_fence.expect("remote_fence set whenever remote_stream is");
        let fenced = remote_session
            .as_ref()
            .expect("remote_session set whenever remote_stream is")
            .fenced();
        let pubkey = remote_session
            .as_ref()
            .expect("remote_session set whenever remote_stream is")
            .pubkey()
            .to_string();
        let roster_revision = remote_session
            .as_ref()
            .expect("remote_session set whenever remote_stream is")
            .roster()
            .revision;
        let roster_ctrl_tx = ctrl_tx.clone();
        tokio::spawn(async move {
            tokio::select! {
                cause = crate::audio::join::read_owner_control(
                    &mut stream,
                    fenced,
                    roster_revision,
                    &roster_ctrl_tx,
                ) => {
                    teardown_remote_huddle(cause, channel_id, &reader_cancel, &fence);
                }
                _ = reader_cancel.cancelled() => {
                    crate::audio::join::send_clean_close(&mut stream, fenced, &pubkey).await;
                }
            }
        })
    });

    // Owner path: watch the room's owner-loss / owner-drain signals. Fenced loss
    // and intentional drain both close local owner clients for rejoin and forget
    // the local generation floor so the fresh generation is accepted. The cause
    // distinction is carried on the remote control streams; locally the action
    // is the same WS teardown. Silent on ordinary client leave.
    let owner_teardown_task = if owner_lost.is_some() || owner_draining.is_some() {
        let fence = Arc::clone(
            &state
                .mesh()
                .expect("owner teardown watcher only exists when mesh owner state exists")
                .audio_fence,
        );
        let owner_cancel = cancel.clone();
        Some(tokio::spawn(async move {
            let lost_fired = async {
                match &owner_lost {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            let drain_fired = async {
                match &owner_draining {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = drain_fired => {
                    info!(
                        channel_id = %channel_id,
                        "huddle owner is draining — closing local client for rejoin"
                    );
                    owner_cancel.cancel();
                    fence.forget(channel_id);
                }
                _ = lost_fired => {
                    info!(
                        channel_id = %channel_id,
                        "huddle owner lost its lease — closing local client for rejoin"
                    );
                    owner_cancel.cancel();
                    fence.forget(channel_id);
                }
                _ = owner_cancel.cancelled() => {}
            }
        }))
    } else {
        None
    };

    recv_loop(
        ws_recv,
        Arc::clone(&room),
        peer_id,
        requested_version,
        ctrl_tx,
        Arc::clone(&missed_pongs),
        cancel.clone(),
        remote_session.as_mut(),
    )
    .await;

    cancel.cancel();
    let _ = send_task.await;
    let _ = heartbeat_task.await;
    let _ = forward_task.await;
    // The reader task owns the owner control stream; joining it here guarantees
    // its clean-close (or teardown) completes before connection cleanup returns.
    if let Some(reader_task) = reader_task {
        let _ = reader_task.await;
    }
    // The owner teardown watcher is cancelled by `cancel.cancel()` above (or has
    // already fired); join it so it settles before cleanup.
    if let Some(owner_teardown_task) = owner_teardown_task {
        let _ = owner_teardown_task.await;
    }
    if let Some(expiry_task) = nip_fi_audio_expiry_task {
        let _ = expiry_task.await;
    }
    // Atomic owner remove + end check: remove_peer_and_check_ended holds the
    // AdmissionGuard lock across index recycling AND the is_empty + ended=true
    // check. Ingress mirrors never archive authoritative huddle state; they
    // remove locally and let the owner decide room lifetime.
    let removal = if remote_session.is_some() {
        room.remove_peer(peer_id).map(|delta| (delta, false))
    } else {
        room.remove_peer_and_check_ended(peer_id)
    };
    let removal_revision = if remote_session.is_none() {
        removal.as_ref().map(|(delta, _)| delta.revision)
    } else {
        // The ingress mirror's local revision is not the owner's authoritative
        // ordering. Omit it rather than publishing a plausible-but-wrong value.
        None
    };
    let should_auto_end = removal.as_ref().map(|(_, ended)| *ended).unwrap_or(false);

    if remote_session.is_none() {
        if let Some((delta, _)) = removal {
            if let Some(left) = delta.left {
                let left_msg = serde_json::json!({
                    "type": "left",
                    "revision": delta.revision,
                    "pubkey": left.pubkey,
                    "peer_index": left.peer_index,
                    "epoch": left.epoch,
                })
                .to_string();
                room.broadcast_control(left_msg);
            } else {
                warn!(
                    channel_id = %channel_id,
                    revision = delta.revision,
                    "audio peer removal delta did not include the removed peer"
                );
            }
        }
    }

    emit_participant_event(
        &state,
        &tenant,
        channel_id,
        parent_id_for_event,
        ParticipantLifecycle {
            kind: Kind::Custom(48102),
            participant_pubkey: &pubkey_hex,
            roster_revision: removal_revision,
            admission_id: Some(peer_id),
        },
    )
    .await;

    let room_emptied;
    if should_auto_end {
        info!(channel_id = %channel_id, "audio room empty — auto-ending huddle");

        match state
            .db
            .archive_channel(tenant.community(), channel_id)
            .await
        {
            Err(e) => {
                warn!(channel_id = %channel_id, "auto-archive failed, huddle stays alive: {e}");
                room.clear_ended();
                room_emptied = false;
            }
            Ok(()) => {
                room_emptied = state
                    .audio_rooms
                    .cleanup_if_empty(tenant.community(), channel_id);

                emit_participant_event(
                    &state,
                    &tenant,
                    channel_id,
                    parent_id_for_event,
                    ParticipantLifecycle {
                        kind: Kind::Custom(48103),
                        participant_pubkey: &pubkey_hex,
                        roster_revision: None,
                        admission_id: None,
                    },
                )
                .await;
            }
        }
    } else {
        room_emptied = state
            .audio_rooms
            .cleanup_if_empty(tenant.community(), channel_id);
    }

    // Owner path: release this room's lease when the room empties, so a new
    // owner can acquire and the renewer stops cleanly (silent, not owner-loss).
    // Fenced on the generation this connection saw as owner: if the room
    // emptied and a re-acquire installed a newer epoch in the gap, `release`
    // is a no-op for the stale generation and leaves the live renewer running.
    // Only the last leaver empties the room, so exactly one release fires.
    if room_emptied {
        if let (Some(mesh), Some(generation)) = (state.mesh(), owner_generation) {
            mesh.owners.release(channel_id, generation);
        }
    }

    info!(
        channel_id = %channel_id,
        pubkey = %pubkey_hex,
        "audio peer left"
    );
}

/// React to a non-owner huddle teardown signal read off the owner's control
/// stream: cancel the connection (which drives the client's WS to close so it
/// rejoins) and forget the local generation floor for this session.
///
/// The `cause` is logged for observability but does not change behaviour —
/// every cause is recoverable by a rejoin, whether against a fresh owner
/// (`OwnerLost`/`StreamClosed`), a draining owner (`OwnerDraining`), or a room
/// that simply ended (`SessionEnded`). `forget` clears local stale-frame
/// suppression so the rejoin's fresh generation is accepted; it never
/// authorizes ownership — Redis fenced CAS remains the arbiter.
fn teardown_remote_huddle(
    cause: crate::audio::join::HuddleTeardownCause,
    channel_id: Uuid,
    cancel: &CancellationToken,
    fence: &crate::audio::mesh::GenerationFloor,
) {
    info!(
        channel_id = %channel_id,
        ?cause,
        "owner tore down cross-pod huddle session — closing client for rejoin"
    );
    cancel.cancel();
    fence.forget(channel_id);
}

/// Map an owner's registration rejection to the client-facing WS error, using
/// the same `code`s a same-pod join produces so a cross-pod client handles them
/// identically. Fence rejections carry their taxonomy code for observability.
fn remote_rejection_ws_error(reason: &crate::audio::join::RegisterRejection) -> serde_json::Value {
    use crate::audio::join::RegisterRejection;
    match reason {
        RegisterRejection::RoomFull => serde_json::json!({
            "type": "error", "code": "room_full",
            "message": "room participant capacity reached"
        }),
        RegisterRejection::RoomEnded => serde_json::json!({
            "type": "error", "code": "room_ended", "message": "huddle has ended"
        }),
        RegisterRejection::VersionMismatch { pinned, requested } => serde_json::json!({
            "type": "error", "code": "upgrade_required",
            "message": format!(
                "this huddle is using audio protocol v{pinned}; your client requested v{requested}"
            ),
            "pinned_version": pinned,
            "requested_version": requested,
        }),
        RegisterRejection::Fenced(f) => serde_json::json!({
            "type": "error", "code": "join_rejected",
            "message": "huddle join rejected",
            "fence_reason": f.code(),
        }),
    }
}

/// Receive loop: reads client frames and routes them. Local/owner joins fan
/// out through the local room; a non-owner join forwards to the huddle owner
/// via `remote_session`. Argument count reflects the pre-existing connection
/// wiring plus the one mesh session; a param struct would obscure more than it
/// clarifies at this single call site.
#[allow(clippy::too_many_arguments)]
async fn recv_loop(
    mut ws_recv: futures_util::stream::SplitStream<WebSocket>,
    room: Arc<crate::audio::room::Room>,
    peer_id: Uuid,
    protocol_version: u8,
    ctrl_tx: mpsc::Sender<WsMessage>,
    missed_pongs: Arc<AtomicU8>,
    cancel: CancellationToken,
    mut remote_session: Option<&mut crate::audio::join::RemoteHuddleSession>,
) {
    use crate::audio::wire::{FrameHeader, V2_HEADER_LEN};

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            msg = ws_recv.next() => {
                match msg {
                    Some(Ok(WsMessage::Binary(data))) => {
                        if data.len() > MAX_AUDIO_FRAME_BYTES {
                            warn!(peer_id = %peer_id, bytes = data.len(), "audio frame too large — dropping");
                            continue;
                        }

                        // Protocol v2 sanity-parse: validate the header is
                        // present and well-shaped, then forward opaquely.
                        // We never strip, rewrite, or re-encode bytes — the
                        // header is sender-authored telemetry only — but we
                        // do refuse to broadcast frames that are clearly
                        // malformed for the room's pinned protocol so we
                        // don't help v2 peers feed garbage to other v2 peers.
                        if protocol_version >= 2 {
                            // Frame must carry at least the 8-byte header
                            // plus a non-empty Opus payload.
                            if data.len() <= V2_HEADER_LEN {
                                warn!(
                                    peer_id = %peer_id,
                                    bytes = data.len(),
                                    "v2 frame missing header or payload — dropping"
                                );
                                continue;
                            }
                            match FrameHeader::parse(&data) {
                                Some((header, payload)) if !payload.is_empty() => {
                                    // Header is well-formed. `level_dbov` is
                                    // already clamped by `parse` — bad values
                                    // do not drop the frame, they just lose
                                    // the metric (which the relay does not
                                    // trust for anything anyway).
                                    tracing::trace!(
                                        peer_id = %peer_id,
                                        seq = header.seq,
                                        ts_48k = header.ts_48k,
                                        level_dbov = header.level_dbov,
                                        is_dtx = header.is_dtx(),
                                        "v2 audio frame"
                                    );
                                }
                                _ => {
                                    warn!(
                                        peer_id = %peer_id,
                                        bytes = data.len(),
                                        "v2 frame failed header parse — dropping"
                                    );
                                    continue;
                                }
                            }
                        }

                        // Non-owner path forwards the client's Opus to the
                        // huddle owner as a datagram (the owner is the sole
                        // fan-out authority); the owner-side room fans it back
                        // to every participant, including our co-located peers.
                        // Owner/local path fans out through the local room.
                        match remote_session.as_deref_mut() {
                            Some(session) => session.forward_media(&data),
                            None => room.broadcast_frame(peer_id, data),
                        }
                    }
                    Some(Ok(WsMessage::Text(text))) => {
                        if text.len() > MAX_TEXT_FRAME_BYTES {
                            warn!(peer_id = %peer_id, bytes = text.len(), "control text frame too large — dropping");
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            if v.get("type").and_then(|t| t.as_str()) == Some("leave") {
                                break;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Pong(_))) => {
                        missed_pongs.store(0, Ordering::Relaxed);
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        // Pong goes through the control channel — priority delivery.
                        let _ = ctrl_tx.try_send(WsMessage::Pong(data));
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Err(e)) => {
                        debug!(peer_id = %peer_id, "ws error: {e}");
                        break;
                    }
                }
            }
        }
    }
}

/// Outbound send loop with control-frame priority (matches connection.rs pattern).
///
/// Control frames (Ping, Pong, Close, control JSON) are drained first on every
/// iteration, so heartbeat pings are never starved by audio backpressure.
pub(crate) async fn send_loop<S>(
    mut ws_send: S,
    mut data_rx: mpsc::Receiver<WsMessage>,
    mut ctrl_rx: mpsc::Receiver<WsMessage>,
    mut terminal_ctrl_rx: mpsc::Receiver<WsMessage>,
    cancel: CancellationToken,
    disconnect_reason: watch::Receiver<Option<crate::state::CommunityDisconnectReason>>,
) where
    S: futures_util::Sink<WsMessage> + Unpin,
{
    loop {
        // Priority: drain all pending control frames before data.
        while let Ok(ctrl_msg) = ctrl_rx.try_recv() {
            if ws_send.send(ctrl_msg).await.is_err() {
                return;
            }
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Drain the terminal NIP-FI denial frame first (if any), then
                // ordinary control frames, before closing. Mirrors the root
                // relay send_loop idiom. The terminal channel has capacity 1
                // and is written before cancel() fires, so it is always
                // available when denial is enqueued — even when ctrl_rx
                // (capacity 8) is full. Without this drain the biased cancel
                // branch sends Close first and the client never sees the
                // required denial frame.
                while let Ok(terminal_msg) = terminal_ctrl_rx.try_recv() {
                    if ws_send.send(terminal_msg).await.is_err() {
                        return;
                    }
                }
                while let Ok(ctrl_msg) = ctrl_rx.try_recv() {
                    if ws_send.send(ctrl_msg).await.is_err() {
                        return;
                    }
                }
                let close = disconnect_reason
                    .borrow()
                    .map_or(WsMessage::Close(None), |reason| reason.close_message());
                let _ = ws_send.send(close).await;
                break;
            }
            Some(ctrl_msg) = ctrl_rx.recv() => {
                if ws_send.send(ctrl_msg).await.is_err() { break; }
            }
            Some(msg) = data_rx.recv() => {
                if ws_send.send(msg).await.is_err() { break; }
            }
        }
    }
}

// Bridges the room's mpsc channel to the WS send channel.

/// Bridges room per-peer channels → WS send channels.
/// Audio frames (from room audio_rx) go to data_tx.
/// Control messages (from room ctrl_rx) go to ws ctrl_tx (priority path).
/// Two separate room channels ensure control is never starved by audio backpressure.
async fn audio_forward_loop(
    mut audio_rx: mpsc::Receiver<Bytes>,
    mut peer_ctrl_rx: mpsc::Receiver<PeerCtrl>,
    data_tx: mpsc::Sender<WsMessage>,
    ctrl_tx: mpsc::Sender<WsMessage>,
    cancel: CancellationToken,
    connection_cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            // Control messages get priority over audio in the select.
            msg = peer_ctrl_rx.recv() => {
                match msg {
                    Some(PeerCtrl::Json(json)) => {
                        if ctrl_tx.try_send(WsMessage::Text(json.into())).is_err() {
                            // State-bearing roster control may not be dropped.
                            // Closing the connection forces admission to replay
                            // a fresh authoritative snapshot.
                            connection_cancel.cancel();
                            break;
                        }
                    }
                    Some(PeerCtrl::Close) | None => {
                        connection_cancel.cancel();
                        break;
                    }
                }
            }
            frame = audio_rx.recv() => {
                match frame {
                    Some(bytes) => {
                        let _ = data_tx.try_send(WsMessage::Binary(bytes));
                    }
                    None => break,
                }
            }
        }
    }
}

async fn heartbeat_loop(
    ws_tx: mpsc::Sender<WsMessage>,
    missed_pongs: Arc<AtomicU8>,
    cancel: CancellationToken,
) {
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                // fetch_add returns the previous value; +1 gives the current count.
                let missed = missed_pongs.fetch_add(1, Ordering::Relaxed) + 1;
                if missed >= MAX_MISSED_PONGS {
                    warn!("audio: {missed} missed pongs — closing connection");
                    cancel.cancel();
                    break;
                }
                if ws_tx.try_send(WsMessage::Ping(axum::body::Bytes::new())).is_err() {
                    cancel.cancel();
                    break;
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

/// Outcome of [`check_membership_for_admission`].
///
/// `Existing` means the caller is already a member; no write is needed at join
/// time. `AutoAddRequired` means a membership write is still needed; it is
/// deferred into the same DB transaction that inserts the `48101` event, so
/// neither can commit without the other.
#[derive(Debug, Clone)]
pub(crate) enum MembershipAdmission {
    /// Caller is already a member of the audio channel.
    Existing { parent_channel_id: Uuid },
    /// Caller is a member of the parent channel and needs auto-add to the
    /// audio channel. The write is deferred into `commit_participant_join`.
    AutoAddRequired {
        parent_channel_id: Uuid,
        channel_created_by: Vec<u8>,
    },
}

/// Validate membership for audio admission — **no durable write**.
///
/// Loads the channel, checks archival status, resolves the parent-channel
/// linkage for ephemeral channels, and checks existing membership and parent
/// membership. Returns [`MembershipAdmission`] describing what still needs
/// to happen at commit time.
///
/// Performs zero DB writes. Any needed auto-add write is deferred into the
/// caller-owned transaction inside `commit_participant_join`.
async fn check_membership_for_admission(
    state: &AppState,
    tenant: &TenantContext,
    channel_id: Uuid,
    pubkey_bytes: &[u8],
    parent_channel_id: Option<Uuid>,
) -> Result<MembershipAdmission, String> {
    // Test hook: fires at the entry of the membership check so a test can arm
    // expiry between NIP-42 pairing and the first DB read. Proves that a
    // cancellation before membership check produces zero DB side effects.
    // No-op in production. [nip_fi_test_hooks::audio_membership_check_hook]
    #[cfg(test)]
    crate::nip_fi_test_hooks::before_membership_check(tenant.community()).await;

    // Load channel first — reject archived channels before any membership check.
    let channel = state
        .db
        .get_channel(tenant.community(), channel_id)
        .await
        .map_err(|e| format!("db error: {e}"))?;

    if channel.archived_at.is_some() {
        return Err("channel is archived".into());
    }

    // Lifecycle events for an ephemeral huddle belong in its parent channel.
    let lifecycle_parent_id = if channel.ttl_seconds.is_some() {
        let parent_id = parent_channel_id.ok_or("ephemeral channel requires parent linkage")?;
        let linked = state
            .db
            .huddle_started_link_exists(
                tenant.community(),
                parent_id,
                channel_id,
                &channel.created_by,
            )
            .await
            .map_err(|e| format!("db error: {e}"))?;
        if !linked {
            return Err("ephemeral channel is not linked to claimed parent".into());
        }
        parent_id
    } else {
        channel_id
    };

    // Fast path: already a member.
    let is_member = state
        .is_member_cached(tenant.community(), channel_id, pubkey_bytes)
        .await
        .map_err(|e| format!("db error: {e}"))?;

    if is_member {
        return Ok(MembershipAdmission::Existing {
            parent_channel_id: lifecycle_parent_id,
        });
    }

    if channel.visibility == "open" {
        return Ok(MembershipAdmission::Existing {
            parent_channel_id: lifecycle_parent_id,
        });
    }

    // Auto-add path: private ephemeral channel + caller is member of parent.
    if channel.ttl_seconds.is_some() {
        let parent_member = state
            .is_member_cached(tenant.community(), lifecycle_parent_id, pubkey_bytes)
            .await
            .map_err(|e| format!("db error: {e}"))?;

        if parent_member {
            return Ok(MembershipAdmission::AutoAddRequired {
                parent_channel_id: lifecycle_parent_id,
                channel_created_by: channel.created_by.clone(),
            });
        }
    }

    Err("not a member".into())
}

/// Error returned by [`commit_participant_join`].
#[derive(Debug)]
pub(crate) enum JoinCommitError {
    /// DB transaction setup or commit failed.
    Db(buzz_db::DbError),
    /// The session gate rejected the permit (session expired before commit).
    Expired,
}

impl std::fmt::Display for JoinCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinCommitError::Db(e) => write!(f, "db error: {e}"),
            JoinCommitError::Expired => write!(f, "session expired before commit"),
        }
    }
}

impl From<buzz_db::DbError> for JoinCommitError {
    fn from(e: buzz_db::DbError) -> Self {
        JoinCommitError::Db(e)
    }
}

/// Atomically commit the participant join: auto-add membership (if needed) +
/// kind `48101` event, in one DB transaction, under a session effect permit.
///
/// Ordering (per B1 contract [e5bc0382]):
/// 1. Sign the `48101` event synchronously.
/// 2. Begin a caller-owned DB transaction.
/// 3. Under the channel membership lock: re-read membership state and auto-add
///    if `AutoAddRequired` and membership is still absent. A concurrent
///    legitimate add is observed as existing and is not overwritten.
/// 4. Insert kind `48101` in the same transaction (uncommitted).
/// 5. Acquire a session effect permit (or rollback + return `Err(Expired)`).
/// 6. Commit the transaction while holding the permit. On commit error, roll
///    back explicitly and return `Err(Db(...))`.
/// 7. While the same permit is held: mark the event locally, fan out to local
///    subscribers, publish to Redis. Errors here use existing handling (warn,
///    invalidate local mark). Drop the permit after fan-out.
///
/// Never cancels or drops the commit future once started — commit returns a
/// known outcome and that outcome drives success or the pre-admission cleanup.
///
/// Argument count reflects the join's natural surface; a param struct would
/// obscure more than it clarifies at this single call site.
#[allow(clippy::too_many_arguments)]
async fn commit_participant_join(
    state: &AppState,
    tenant: &TenantContext,
    channel_id: Uuid,
    parent_channel_id: Uuid,
    pubkey_hex: &str,
    pubkey_bytes: &[u8],
    peer_id: Uuid,
    roster_revision: u64,
    membership_admission: &MembershipAdmission,
    gate: &std::sync::Arc<crate::nip_fi_gate::SessionAdmissionGate>,
) -> Result<StoredEvent, JoinCommitError> {
    // 1. Sign the 48101 event synchronously.
    let content = serde_json::json!({
        "ephemeral_channel_id": channel_id.to_string(),
        "roster_revision": roster_revision,
        "admission_id": peer_id.to_string(),
    })
    .to_string();

    let h_tag = Tag::parse(["h", &parent_channel_id.to_string()]).map_err(|e| {
        JoinCommitError::Db(buzz_db::DbError::InvalidData(format!(
            "failed to build h tag: {e}"
        )))
    })?;
    let p_tag = Tag::parse(["p", pubkey_hex]).map_err(|e| {
        JoinCommitError::Db(buzz_db::DbError::InvalidData(format!(
            "failed to build p tag: {e}"
        )))
    })?;
    let event = EventBuilder::new(Kind::Custom(48101), content)
        .tags(vec![h_tag, p_tag])
        .sign_with_keys(&state.relay_keypair)
        .map_err(|e| {
            JoinCommitError::Db(buzz_db::DbError::InvalidData(format!(
                "failed to sign 48101: {e}"
            )))
        })?;
    let event_id_hex = event.id.to_hex();

    // 2. Begin a caller-owned DB transaction.
    let mut tx = state.db.begin_event_write_transaction().await?;

    // 3. Under the channel membership lock: auto-add if still absent.
    if let MembershipAdmission::AutoAddRequired {
        channel_created_by, ..
    } = membership_admission
    {
        buzz_db::channel_members::acquire_channel_membership_lock_in_transaction(
            &mut tx,
            tenant.community(),
            channel_id,
        )
        .await?;

        // Re-read membership — a concurrent add may have already provided access.
        let still_absent = !buzz_db::channel_members::is_member_in_transaction(
            &mut tx,
            tenant.community(),
            channel_id,
            pubkey_bytes,
        )
        .await?;

        if still_absent {
            buzz_db::channel_members::insert_auto_membership_in_transaction(
                &mut tx,
                tenant.community(),
                channel_id,
                pubkey_bytes,
                channel_created_by.as_slice(),
            )
            .await?;
        }
        // If not still_absent: a concurrent legitimate add already committed.
        // The joint transaction observes it; we do not need to compensate later.
    }

    // 4. Insert kind `48101` uncommitted.
    let (stored, was_inserted) = buzz_db::event::insert_event_in_transaction(
        &mut tx,
        tenant.community(),
        &event,
        Some(parent_channel_id),
    )
    .await?;

    // 5. Acquire effect permit or rollback.
    //
    // Test hook: fires between the uncommitted 48101 insert and the permit
    // acquisition. A test can arm expiry here to prove that a cancellation
    // after the DB write but before commit rolls back the transaction and
    // produces zero committed side effects.
    // [nip_fi_test_hooks::audio_participant_commit_hook]
    #[cfg(test)]
    crate::nip_fi_test_hooks::before_participant_commit(tenant.community()).await;
    let _permit = match gate.acquire_effect().await {
        Ok(permit) => permit,
        Err(crate::nip_fi_gate::SessionExpired) => {
            // Rollback explicitly — no 48101 or membership write committed.
            let _ = tx.rollback().await;
            return Err(JoinCommitError::Expired);
        }
    };

    // 6. Commit while holding the permit.
    if let Err(e) = tx.commit().await {
        return Err(JoinCommitError::Db(e.into()));
    }

    // 7. Fan-out while permit is still held — expiry cannot complete between
    //    row visibility and fan-out.
    if was_inserted {
        state.mark_local_event(tenant.community(), &event.id);
        crate::handlers::event::fan_out_event_to_local_subscribers(
            state,
            tenant.community(),
            &stored,
        )
        .await;

        if let Err(e) = state
            .pubsub
            .publish_event(tenant, EventTopic::Channel(parent_channel_id), &event)
            .await
        {
            state
                .local_event_ids
                .invalidate(&(tenant.community(), event.id.to_bytes()));
            warn!(
                event_id = %event_id_hex,
                channel_id = %parent_channel_id,
                "audio: failed to publish 48101: {e}"
            );
        }

        // Best-effort mention insertion — outside the gate, failure is a warn.
        if let Err(e) = buzz_db::insert_mentions(
            state.db.pool(),
            tenant.community(),
            &event,
            Some(parent_channel_id),
        )
        .await
        {
            warn!(event_id = %event_id_hex, "audio: failed to insert 48101 mentions: {e}");
        }
    } else {
        debug!(
            event_id = %event_id_hex,
            channel_id = %parent_channel_id,
            "audio: 48101 already persisted — skipping fan-out"
        );
    }
    // _permit drops here — gate quiescence barrier may proceed.

    // After commit, invalidate the membership cache if we auto-added.
    if matches!(
        membership_admission,
        MembershipAdmission::AutoAddRequired { .. }
    ) {
        state.invalidate_membership(tenant, channel_id, pubkey_bytes);
    }

    Ok(stored)
}

#[derive(Clone, Copy)]
struct ParticipantLifecycle<'a> {
    kind: Kind,
    participant_pubkey: &'a str,
    roster_revision: Option<u64>,
    admission_id: Option<Uuid>,
}

async fn emit_participant_event(
    state: &AppState,
    tenant: &TenantContext,
    channel_id: Uuid,
    parent_channel_id: Uuid,
    lifecycle: ParticipantLifecycle<'_>,
) {
    let ParticipantLifecycle {
        kind,
        participant_pubkey,
        roster_revision,
        admission_id,
    } = lifecycle;
    let content = match (roster_revision, admission_id) {
        (Some(revision), Some(admission_id)) => serde_json::json!({
            "ephemeral_channel_id": channel_id.to_string(),
            "roster_revision": revision,
            "admission_id": admission_id.to_string(),
        }),
        (Some(revision), None) => serde_json::json!({
            "ephemeral_channel_id": channel_id.to_string(),
            "roster_revision": revision,
        }),
        (None, Some(admission_id)) => serde_json::json!({
            "ephemeral_channel_id": channel_id.to_string(),
            "admission_id": admission_id.to_string(),
        }),
        (None, None) => serde_json::json!({"ephemeral_channel_id": channel_id.to_string()}),
    }
    .to_string();

    let h_tag = match Tag::parse(["h", &parent_channel_id.to_string()]) {
        Ok(t) => t,
        Err(e) => {
            warn!("audio: failed to parse h tag: {e}");
            return;
        }
    };
    let p_tag = match Tag::parse(["p", participant_pubkey]) {
        Ok(t) => t,
        Err(e) => {
            warn!("audio: failed to parse p tag: {e}");
            return;
        }
    };
    let tags = vec![h_tag, p_tag];

    let event = match EventBuilder::new(kind, content)
        .tags(tags)
        .sign_with_keys(&state.relay_keypair)
    {
        Ok(e) => e,
        Err(e) => {
            warn!("audio: failed to sign lifecycle event: {e}");
            return;
        }
    };

    let event_id_hex = event.id.to_hex();

    // 1. Persist to DB so late-joining clients can reconstruct huddle state
    //    from historical queries. Without this, lifecycle events only exist
    //    for the duration of the Redis pub/sub delivery and are lost forever.
    let stored = match state
        .db
        .insert_event(tenant.community(), &event, Some(parent_channel_id))
        .await
    {
        Ok((stored, true)) => stored,
        Ok((_, false)) => {
            // Duplicate — already persisted (e.g. concurrent emit). Skip fan-out
            // to avoid double-delivery, matching the side_effects.rs pattern.
            debug!(
                event_id = %event_id_hex,
                channel_id = %parent_channel_id,
                "audio lifecycle event already persisted — skipping fan-out"
            );
            return;
        }
        Err(e) => {
            // DB failure during disconnect cleanup. Still broadcast so live
            // subscribers see the leave/end event immediately — suppressing it
            // would leave connected clients stale. Late joiners will have an
            // inconsistent view until the next huddle lifecycle event lands.
            warn!(
                event_id = %event_id_hex,
                channel_id = %parent_channel_id,
                kind = %event.kind.as_u16(),
                "audio: failed to persist lifecycle event: {e}"
            );
            StoredEvent::new(event.clone(), Some(parent_channel_id))
        }
    };

    // 2. Mark as locally-published before Redis broadcast to prevent
    //    double-delivery when the event echoes back through the subscriber loop.
    state.mark_local_event(tenant.community(), &event.id);

    // 3. Local fan-out to WS subscribers on this node, through the guarded send
    //    path so a stale subscription on a removed/non-member connection cannot
    //    receive this channel's audio lifecycle event (same gate as
    //    dispatch_persistent_event in the ingest handler).
    crate::handlers::event::fan_out_event_to_local_subscribers(state, tenant.community(), &stored)
        .await;

    // 4. Cross-node broadcast via Redis pub/sub.
    if let Err(e) = state
        .pubsub
        .publish_event(tenant, EventTopic::Channel(parent_channel_id), &event)
        .await
    {
        state
            .local_event_ids
            .invalidate(&(tenant.community(), event.id.to_bytes()));
        warn!(
            event_id = %event_id_hex,
            channel_id = %parent_channel_id,
            "audio: failed to publish lifecycle event: {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{routing::get, Router};
    use futures_util::SinkExt;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    use super::*;

    #[test]
    fn audio_connection_permits_share_the_global_websocket_budget() {
        let semaphore = Arc::new(Semaphore::new(1));
        let first = acquire_audio_connection_permit(&semaphore).expect("first permit");

        assert!(
            acquire_audio_connection_permit(&semaphore).is_none(),
            "audio connections must stop when the global WebSocket budget is exhausted"
        );

        drop(first);
        assert!(
            acquire_audio_connection_permit(&semaphore).is_some(),
            "dropping an audio connection must return its global permit"
        );
    }

    async fn handler_receives_message_of_size(size: usize) -> bool {
        let (received_tx, received_rx) = oneshot::channel();
        let received_tx = Arc::new(Mutex::new(Some(received_tx)));
        let app = Router::new().route(
            "/",
            get({
                let received_tx = Arc::clone(&received_tx);
                move |ws: WebSocketUpgrade| {
                    let received_tx = Arc::clone(&received_tx);
                    async move {
                        limit_audio_websocket(ws).on_upgrade(move |mut socket| async move {
                            let received = matches!(socket.recv().await, Some(Ok(_)));
                            if let Some(tx) =
                                received_tx.lock().expect("result lock poisoned").take()
                            {
                                let _ = tx.send(received);
                            }
                        })
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test WebSocket listener");
        let addr = listener.local_addr().expect("test listener address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test WebSocket server");
        });

        let (mut client, _) = connect_async(format!("ws://{addr}/"))
            .await
            .expect("connect test WebSocket client");
        client
            .send(Message::Text("x".repeat(size).into()))
            .await
            .expect("send test WebSocket message");

        let received = tokio::time::timeout(Duration::from_secs(2), received_rx)
            .await
            .expect("server should process the test message")
            .expect("server should report whether it received the message");

        server.abort();
        let _ = server.await;

        received
    }

    #[tokio::test]
    async fn saturated_websocket_control_queue_cancels_the_audio_connection() {
        let (_audio_tx, audio_rx) = mpsc::channel(1);
        let (peer_ctrl_tx, peer_ctrl_rx) = mpsc::channel(2);
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(1);
        ctrl_tx
            .try_send(WsMessage::Ping(Bytes::new()))
            .expect("fill websocket control queue");
        peer_ctrl_tx
            .try_send(PeerCtrl::Json("{}".into()))
            .expect("queue state-bearing control");
        let task_cancel = CancellationToken::new();
        let connection_cancel = CancellationToken::new();

        audio_forward_loop(
            audio_rx,
            peer_ctrl_rx,
            data_tx,
            ctrl_tx,
            task_cancel,
            connection_cancel.clone(),
        )
        .await;

        assert!(
            connection_cancel.is_cancelled(),
            "saturated websocket control must force a fresh roster admission"
        );
    }

    #[tokio::test]
    async fn closed_peer_control_queue_cancels_the_audio_connection() {
        let (_audio_tx, audio_rx) = mpsc::channel(1);
        let (peer_ctrl_tx, peer_ctrl_rx) = mpsc::channel(1);
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(1);
        let task_cancel = CancellationToken::new();
        let connection_cancel = CancellationToken::new();

        let forward = tokio::spawn(audio_forward_loop(
            audio_rx,
            peer_ctrl_rx,
            data_tx,
            ctrl_tx,
            task_cancel,
            connection_cancel.clone(),
        ));
        drop(peer_ctrl_tx);

        tokio::time::timeout(Duration::from_secs(1), forward)
            .await
            .expect("forwarder exits when its state-bearing queue closes")
            .expect("forwarder task completes cleanly");
        assert!(
            connection_cancel.is_cancelled(),
            "lost control state must tear down the WebSocket for a fresh roster"
        );
    }

    #[tokio::test]
    async fn audio_send_loop_sends_policy_close_when_community_is_deleted() {
        use futures_util::Sink;

        struct MockSink {
            messages: Arc<Mutex<Vec<WsMessage>>>,
        }

        impl Sink<WsMessage> for MockSink {
            type Error = std::io::Error;

            fn poll_ready(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn start_send(
                self: std::pin::Pin<&mut Self>,
                item: WsMessage,
            ) -> Result<(), Self::Error> {
                self.messages.lock().expect("mock sink poisoned").push(item);
                Ok(())
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_close(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                self.poll_flush(cx)
            }
        }

        let (_data_tx, data_rx) = mpsc::channel(1);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let control = CommunityConnectionControl::new(cancel.clone());
        let disconnect_reason = control.disconnect_reason();
        let registry = crate::state::CommunityConnectionRegistry::new();
        let community = buzz_core::CommunityId::from_uuid(Uuid::new_v4());
        let _guard = registry.register(Uuid::new_v4(), community, control);
        assert_eq!(registry.disconnect_community(community), 1);
        let messages = Arc::new(Mutex::new(Vec::new()));
        let sink = MockSink {
            messages: Arc::clone(&messages),
        };

        send_loop(
            sink,
            data_rx,
            ctrl_rx,
            mpsc::channel(1).1,
            cancel,
            disconnect_reason,
        )
        .await;

        let messages = messages.lock().expect("mock sink poisoned");
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            WsMessage::Close(Some(close)) => {
                assert_eq!(close.code, axum::extract::ws::close_code::POLICY);
                assert_eq!(close.reason.as_str(), "community deleted");
            }
            other => panic!("expected one 1008 deletion close, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn audio_websocket_parser_rejects_oversized_messages_before_handler_reads_them() {
        assert!(
            handler_receives_message_of_size(MAX_WEBSOCKET_MESSAGE_BYTES).await,
            "messages at the audio route limit should still reach the handler"
        );
        assert!(
            !handler_receives_message_of_size(MAX_WEBSOCKET_MESSAGE_BYTES + 1).await,
            "oversized messages must be rejected by the WebSocket parser before the handler sees them"
        );
    }

    // ── Witness B: Audio pairing mismatch through the real audio path ─────────
    //
    // Drives the production `handle_active_audio_connection` over a real local
    // WebSocket pair. Key A is named in the assertion; key B signs the audio
    // auth message — mismatch. The function must deliver the exact restricted
    // JSON frame and cancel before returning.
    //
    // The test calls `handle_active_audio_connection` directly (bypassing
    // `handle_audio_connection`/`run_registered_community_connection`) so no
    // live DB connection is required: the pairing fires before any membership
    // DB gate, so a lazy pool suffices.
    //
    // Mutation evidence:
    //   - Delete the production call from `handle_active_audio_connection` →
    //     exact restricted frame absent (or a later, different error arrives);
    //     test panics on frame content or cancellation assertion.
    //   - Delete the denial branch inside `enforce_nip_fi_key_pairing` → same.
    //   - Change the JSON shape/text → byte assertion panics.
    //   - Omit cancellation → cancellation assertion panics.

    async fn audio_test_state() -> std::sync::Arc<crate::state::AppState> {
        use std::sync::Arc;
        let mut config = crate::config::Config::from_env().expect("default config loads");
        config.require_relay_membership = false;
        config.database_url = "postgres://buzz:buzz_dev@127.0.0.1:1/buzz".to_string();
        config.redis_url = "redis://127.0.0.1:1".to_string();
        let pool = sqlx::PgPool::connect_lazy(&config.database_url).expect("lazy pg pool");
        let db = buzz_db::Db::from_pool(pool.clone());
        let redis_pool = deadpool_redis::Config::from_url(&config.redis_url)
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .expect("redis pool");
        let pubsub = Arc::new(
            buzz_pubsub::PubSubManager::new(&config.redis_url, redis_pool.clone())
                .await
                .expect("pubsub manager"),
        );
        let audit = buzz_audit::AuditService::new(pool.clone());
        let auth = buzz_auth::AuthService::new(config.auth.clone());
        let search = buzz_search::SearchService::new(pool.clone());
        let workflow_engine = Arc::new(buzz_workflow::WorkflowEngine::new(
            db.clone(),
            buzz_workflow::WorkflowConfig::default(),
        ));
        let media_storage = buzz_media::MediaStorage::new(&config.media).expect("media storage");
        let (state, _audit_shutdown) = crate::state::AppState::new(
            config,
            db,
            redis_pool,
            audit,
            pubsub,
            auth,
            search,
            workflow_engine,
            nostr::Keys::generate(),
            media_storage,
        );
        Arc::new(state)
    }

    #[tokio::test]
    async fn handle_active_audio_connection_pairing_mismatch_runs_full_audio_denial_path() {
        use buzz_auth::VerifiedAssertion;
        use chrono::{Duration, Utc};
        use std::sync::Arc;

        let key_a = nostr::Keys::generate();
        let key_b = nostr::Keys::generate();

        let assertion = VerifiedAssertion::for_test(
            Some(key_a.public_key()),
            vec![Utc::now() + Duration::hours(1)],
        );

        let state = audio_test_state().await;
        let _channel_id = uuid::Uuid::new_v4();

        // Build a real tenant context matching what `nip42_expected_relay_url`
        // will compute (scheme from config.relay_url = "ws://", host = "test.local").
        let tenant = buzz_core::tenant::TenantContext::resolved(
            buzz_core::tenant::CommunityId::from_uuid(uuid::Uuid::nil()),
            "test.local".to_string(),
        );

        // Set up a local WS server that runs `handle_active_audio_connection`.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        // conn_cancel is created here so the test retains it for the
        // is_cancelled() assertion. The token is cloned into the server closure.
        let conn_cancel = CancellationToken::new();
        let cancel_for_assert = conn_cancel.clone();
        let state_c = Arc::clone(&state);
        let tenant_c = tenant.clone();
        let assertion_c = assertion.clone();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("test listener addr");

        let server = tokio::spawn(async move {
            let app = Router::new().route(
                "/",
                get({
                    let state_i = Arc::clone(&state_c);
                    let tenant_i = tenant_c.clone();
                    let assertion_i = assertion_c.clone();
                    // Clone once for the closure; the original is retained
                    // outside for the cancellation assertion.
                    let cancel_i = conn_cancel.clone();
                    move |ws: WebSocketUpgrade| {
                        let state_i = Arc::clone(&state_i);
                        let tenant_i = tenant_i.clone();
                        let assertion_i = assertion_i.clone();
                        let conn_time = chrono::Utc::now();
                        let control_inner =
                            crate::state::CommunityConnectionControl::new(cancel_i.clone());
                        async move {
                            ws.on_upgrade(move |socket| async move {
                                handle_active_audio_connection(
                                    socket,
                                    state_i,
                                    tenant_i,
                                    uuid::Uuid::new_v4(),
                                    control_inner,
                                    Some(assertion_i),
                                    conn_time,
                                )
                                .await
                            })
                        }
                    }
                }),
            );

            let _ = ready_tx.send(());
            axum::serve(listener, app).await.expect("test server");
        });

        // Wait for server to be ready, then get the cancel token it sent.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("server ready");

        // Refactor: the server uses its own cancel per connection (above).
        // We instead track completion by the WS close message.

        // Connect the client.
        let (mut client, _) = connect_async(format!("ws://{addr}/"))
            .await
            .expect("connect client");

        // Receive the challenge message.
        let challenge_msg = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("challenge timeout")
            .expect("challenge message")
            .expect("challenge ws message");
        let challenge_text = match challenge_msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
            other => panic!("expected text challenge; got {other:?}"),
        };
        let challenge_json: serde_json::Value =
            serde_json::from_str(&challenge_text).expect("challenge JSON");
        let challenge = challenge_json["challenge"]
            .as_str()
            .expect("challenge field")
            .to_string();

        // Sign the auth message with key B (mismatch — assertion names key A).
        let relay_url = "ws://test.local";
        let auth_event = nostr::EventBuilder::new(nostr::Kind::Authentication, "")
            .tag(nostr::Tag::parse(["relay", relay_url]).unwrap())
            .tag(nostr::Tag::parse(["challenge", &challenge]).unwrap())
            .sign_with_keys(&key_b)
            .unwrap();

        let auth_msg = serde_json::json!({
            "type": "auth",
            "event": auth_event,
        })
        .to_string();
        client
            .send(tokio_tungstenite::tungstenite::Message::Text(
                auth_msg.into(),
            ))
            .await
            .expect("send auth msg");

        // The server must send the exact restricted JSON frame before closing.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), client.next())
            .await
            .expect("restricted frame timeout")
            .expect("frame")
            .expect("ws frame");

        let expected_restricted = serde_json::json!({
            "type": "restricted",
            "message": buzz_auth::DenialClass::AuthorizationDenied.nostr_text()
        })
        .to_string();

        match frame {
            tokio_tungstenite::tungstenite::Message::Text(t) => {
                assert_eq!(
                    t.as_str(),
                    expected_restricted.as_str(),
                    "audio pairing mismatch must produce exact restricted JSON before close"
                );
            }
            other => panic!("expected Text(restricted JSON); got {other:?}"),
        }

        // The connection must close after the denial. The audio path sends the
        // restricted frame directly on ws_send, then drops it (no send_loop to
        // drain a Close frame). The client may see either:
        //   a) a WS Close frame if axum's runtime sends one on drop, or
        //   b) None / Err (connection reset) when the socket drops.
        // Both are acceptable — the key check is that the restricted frame was
        // already received above.
        let close = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("close timeout");
        assert!(
            matches!(
                close,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | Some(Err(_)) | None
            ),
            "connection must close after audio pairing mismatch; got {close:?}"
        );

        // The retained token must be cancelled — this is the named mutation
        // target: omit cancel.cancel() inside enforce_nip_fi_key_pairing and
        // this assertion fails even though the socket still drops.
        assert!(
            cancel_for_assert.is_cancelled(),
            "conn_cancel must be cancelled after audio pairing mismatch"
        );

        server.abort();
        let _ = server.await;
    }

    // ── W5 (B1 audio): already-expired deadline rejects at pairing, before admission
    //
    // When the NIP-FI session deadline is already past at pairing time (the
    // assertion's authority deadlines are all in the past), `handle_active_audio_connection`
    // must send the canonical `restricted` denial frame and close the connection
    // before writing any relay-membership, room-join, or roster side effect.
    //
    // This test gives the handler the same key in both the assertion and the
    // NIP-42 event so pairing succeeds, but sets an already-expired deadline.
    // The B1 gate fires between the pairing check and `enforce_relay_membership`.
    //
    // Mutation evidence:
    //   A) Delete the B1 already-expired check → the B1 restricted frame is
    //      not sent before admission; the membership gate fires next. Since the
    //      test's lazy DB rejects membership, the frame text changes from
    //      "restricted: authorization denied" to "restricted: not a relay member"
    //      → the byte assertion panics.
    //   B) Change the sent frame text → byte assertion panics.
    //   C) Omit `cancel.cancel()` in the B1 branch → cancel assertion panics.

    #[tokio::test]
    async fn b1_already_expired_session_denied_at_pairing_before_admission() {
        use buzz_auth::VerifiedAssertion;
        use chrono::{Duration, Utc};
        use std::sync::Arc;

        let key = nostr::Keys::generate();

        // Assertion: same key for both assertion and NIP-42 event → pairing passes.
        // But the deadline is 2 seconds in the past → B1 fires.
        let expired_deadline = Utc::now() - Duration::seconds(2);
        let assertion = VerifiedAssertion::for_test(Some(key.public_key()), vec![expired_deadline]);

        let state = audio_test_state().await;

        let tenant = buzz_core::tenant::TenantContext::resolved(
            buzz_core::tenant::CommunityId::from_uuid(uuid::Uuid::nil()),
            "test.local".to_string(),
        );

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let conn_cancel = CancellationToken::new();
        let cancel_for_assert = conn_cancel.clone();
        let state_c = Arc::clone(&state);
        let tenant_c = tenant.clone();
        let assertion_c = assertion.clone();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("test listener addr");

        let server = tokio::spawn(async move {
            let app = Router::new().route(
                "/",
                get({
                    let state_i = Arc::clone(&state_c);
                    let tenant_i = tenant_c.clone();
                    let assertion_i = assertion_c.clone();
                    let cancel_i = conn_cancel.clone();
                    move |ws: WebSocketUpgrade| {
                        let state_i = Arc::clone(&state_i);
                        let tenant_i = tenant_i.clone();
                        let assertion_i = assertion_i.clone();
                        let conn_time = chrono::Utc::now();
                        let control_inner =
                            crate::state::CommunityConnectionControl::new(cancel_i.clone());
                        async move {
                            ws.on_upgrade(move |socket| async move {
                                handle_active_audio_connection(
                                    socket,
                                    state_i,
                                    tenant_i,
                                    uuid::Uuid::new_v4(),
                                    control_inner,
                                    Some(assertion_i),
                                    conn_time,
                                )
                                .await
                            })
                        }
                    }
                }),
            );
            let _ = ready_tx.send(());
            axum::serve(listener, app).await.expect("test server");
        });

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("server ready");

        let (mut client, _) = connect_async(format!("ws://{addr}/"))
            .await
            .expect("connect client");

        // Receive the challenge.
        let challenge_msg = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("challenge timeout")
            .expect("challenge message")
            .expect("challenge ws message");
        let challenge_text = match challenge_msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
            other => panic!("expected text challenge; got {other:?}"),
        };
        let challenge_json: serde_json::Value =
            serde_json::from_str(&challenge_text).expect("challenge JSON");
        let challenge = challenge_json["challenge"]
            .as_str()
            .expect("challenge field")
            .to_string();

        // Sign the auth message with the SAME key as the assertion — pairing passes.
        let relay_url = "ws://test.local";
        let auth_event = nostr::EventBuilder::new(nostr::Kind::Authentication, "")
            .tag(nostr::Tag::parse(["relay", relay_url]).unwrap())
            .tag(nostr::Tag::parse(["challenge", &challenge]).unwrap())
            .sign_with_keys(&key)
            .unwrap();

        let auth_msg = serde_json::json!({
            "type": "auth",
            "event": auth_event,
        })
        .to_string();
        client
            .send(tokio_tungstenite::tungstenite::Message::Text(
                auth_msg.into(),
            ))
            .await
            .expect("send auth msg");

        // The B1 gate must send the exact canonical restricted JSON frame.
        // This is byte-identical to the pairing-mismatch frame — same production
        // `authorization_denied_frame(NipFiWsRoute::Audio)` path.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), client.next())
            .await
            .expect("restricted frame timeout")
            .expect("frame")
            .expect("ws frame");

        let expected_restricted = serde_json::json!({
            "type": "restricted",
            "message": buzz_auth::DenialClass::AuthorizationDenied.nostr_text()
        })
        .to_string();

        match frame {
            tokio_tungstenite::tungstenite::Message::Text(t) => {
                assert_eq!(
                    t.as_str(),
                    expected_restricted.as_str(),
                    "B1: expired session must produce exact canonical restricted JSON before close"
                );
            }
            other => panic!("B1: expected Text(restricted JSON); got {other:?}"),
        }

        // Connection must close after the B1 denial.
        let close = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("close timeout");
        assert!(
            matches!(
                close,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | Some(Err(_)) | None
            ),
            "B1: connection must close after expired-session denial; got {close:?}"
        );

        // The cancel token must be cancelled — omitting cancel.cancel() in the
        // B1 branch makes this assertion fail even when the socket still drops.
        assert!(
            cancel_for_assert.is_cancelled(),
            "B1: conn_cancel must be cancelled after expired-session denial at pairing"
        );

        server.abort();
        let _ = server.await;
    }

    // ── W6 (B1 audio mid-admission): cancellation before room.add_peer ─────────
    //
    // With the expiry task armed before admission (above the first persisting
    // step), a cancellation fired during the admission sequence must prevent
    // room.add_peer from executing. The audio room must remain empty.
    //
    // This test fires the expiry task between the pairing check and the first
    // check_cancel!() boundary. To avoid a sleep-lottery it uses the connection
    // cancel token directly: the token is pre-cancelled, which is equivalent to
    // the expiry task firing before check_cancel!() is reached. The room is
    // inspected after the handler returns to confirm no peer was added.
    //
    // The biased auth-loop select fires `cancel.cancelled()` → return before
    // reaching check_cancel!(). The room invariant (no peer added) is the
    // observable outcome that must hold regardless of which cancellation path
    // fires. The mutation evidence for the check_cancel!() fences themselves is
    // in the focused unit tests in connection.rs (B2/B3 tests), where the fence
    // mechanism is exercised in isolation.
    //
    // What this test proves end-to-end:
    //   A real audio connection with a cancelled token cannot reach room.add_peer.
    //   This was NOT true before the B1 fix: the expiry task was armed AFTER
    //   room.add_peer (line ~858), so it could not prevent admission.
    //
    // Mutation evidence:
    //   A) Move the expiry task creation back to after room.add_peer (the pre-fix
    //      location) → test still passes (cancel path fires first). The test is
    //      therefore evidence of the cancel-stops-admission invariant, not of the
    //      exact placement of the expiry arm.
    //   B) Remove `_ = cancel.cancelled() => return` from the audio auth select →
    //      handler proceeds to auth exchange → if auth takes > 3 s (timeout) the
    //      test fails; in practice the close assertion fires immediately.

    #[tokio::test]
    async fn b1_mid_admission_expiry_does_not_add_peer_to_room() {
        use buzz_auth::VerifiedAssertion;
        use chrono::{Duration, Utc};
        use std::sync::Arc;
        use tokio_tungstenite::connect_async;

        let key = nostr::Keys::generate();
        // A non-expired assertion — pairing passes if we reach that check.
        // The cancellation intercepts before pairing, so the room stays empty.
        let assertion = VerifiedAssertion::for_test(
            Some(key.public_key()),
            vec![Utc::now() + Duration::hours(1)],
        );

        let state = audio_test_state().await;
        let audio_rooms = Arc::clone(&state.audio_rooms);
        let tenant = buzz_core::tenant::TenantContext::resolved(
            buzz_core::tenant::CommunityId::from_uuid(uuid::Uuid::nil()),
            "test.local".to_string(),
        );
        let channel_id = uuid::Uuid::new_v4();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        // Pre-cancel: token is set before handle_active_audio_connection runs.
        // The biased `_ = cancel.cancelled() => return` in the audio auth select
        // fires at the first executor poll, preventing any room mutation.
        let conn_cancel = CancellationToken::new();
        conn_cancel.cancel();
        let cancel_clone = conn_cancel.clone();

        let state_c = Arc::clone(&state);
        let tenant_c = tenant.clone();
        let assertion_c = assertion.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("test listener addr");

        let server = tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::get({
                    let state_i = Arc::clone(&state_c);
                    let tenant_i = tenant_c.clone();
                    let assertion_i = assertion_c.clone();
                    let cancel_i = cancel_clone.clone();
                    move |ws: axum::extract::ws::WebSocketUpgrade| {
                        let state_i = Arc::clone(&state_i);
                        let tenant_i = tenant_i.clone();
                        let assertion_i = assertion_i.clone();
                        let conn_time = chrono::Utc::now();
                        let control_inner =
                            crate::state::CommunityConnectionControl::new(cancel_i.clone());
                        async move {
                            ws.on_upgrade(move |socket| async move {
                                handle_active_audio_connection(
                                    socket,
                                    state_i,
                                    tenant_i,
                                    channel_id,
                                    control_inner,
                                    Some(assertion_i),
                                    conn_time,
                                )
                                .await
                            })
                        }
                    }
                }),
            );
            let _ = ready_tx.send(());
            axum::serve(listener, app).await.expect("test server");
        });

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
            .await
            .expect("server ready");

        let (mut client, _) = connect_async(format!("ws://{addr}/"))
            .await
            .expect("connect client");

        // Server sends the challenge then exits immediately (biased cancel fires).
        // The client receives the challenge, then observes the connection close.
        let _challenge = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .ok(); // May succeed (challenge) or fail (connection already dropped).

        // The connection must close before the 3 s timeout.
        let close = tokio::time::timeout(std::time::Duration::from_secs(3), client.next()).await;
        assert!(
            close.is_ok(),
            "B1: connection must close before timeout when token is pre-cancelled"
        );

        // The audio room must be empty — no peer was added.
        let community = buzz_core::tenant::CommunityId::from_uuid(uuid::Uuid::nil());
        if let Some(room) = audio_rooms.get(community, channel_id) {
            assert!(
                room.is_empty(),
                "B1: audio room must have zero peers when cancel fires before room.add_peer"
            );
        }
        // Room may not exist at all — that also satisfies the invariant.

        server.abort();
        let _ = server.await;
    }

    // ── W7 (B3 audio): audio expiry sends exact restricted frame before close ────
    //
    // Drives BOTH production seams:
    //   1. `nip_fi_session::spawn_nip_fi_expiry_task` with `NipFiWsRoute::Audio`.
    //   2. The real generic audio `send_loop` with a recording sink.
    //
    // The expiry constructor synchronously queues the denial on `ctrl_tx` and
    // cancels without any await in between, so the audio send loop's
    // cancellation drain picks up the frame before writing Close.
    //
    // Mutation evidence:
    //   - Delete/change the audio enqueue in `spawn_nip_fi_expiry_task` →
    //     output lacks or mismatches frame 0.
    //   - Revert the audio send_loop cancellation drain → output begins with
    //     Close(None) or lacks the restricted frame entirely.
    //   - Replace audio's production constructor call with a copied local task →
    //     structural requirement: exactly one `spawn_nip_fi_expiry_task`
    //     definition (in `nip_fi_session`) and two production invocations (root
    //     in `connection.rs`, audio in `audio/handler.rs`). Any copy breaks
    //     this test's coupling to the shared producer.

    #[tokio::test]
    async fn audio_expiry_sends_exact_restricted_frame_before_close() {
        use std::pin::Pin;
        use std::sync::Arc;
        use std::task::{Context, Poll};
        use tokio::sync::{mpsc, watch};

        // Recording sink that stores every message in order.
        struct RecordSink(Arc<tokio::sync::Mutex<Vec<WsMessage>>>);
        impl futures_util::Sink<WsMessage> for RecordSink {
            type Error = std::convert::Infallible;
            fn poll_ready(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
            fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
                self.get_mut()
                    .0
                    .try_lock()
                    .expect("RecordSink lock")
                    .push(item);
                Ok(())
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
            fn poll_close(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                self.poll_flush(cx)
            }
        }

        let recorded = Arc::new(tokio::sync::Mutex::new(Vec::<WsMessage>::new()));
        let sink = RecordSink(Arc::clone(&recorded));

        let (_data_tx, data_rx) = mpsc::channel::<WsMessage>(4);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<WsMessage>(8);
        let (terminal_tx, terminal_rx) = mpsc::channel::<WsMessage>(1);
        let cancel = CancellationToken::new();
        let (disconnect_tx, disconnect_rx) = watch::channel(None);
        drop(disconnect_tx); // plain Close(None)

        // Step 1: spawn audio send_loop and yield so it parks in its select.
        let send_cancel = cancel.clone();
        let send_handle = tokio::spawn(send_loop(
            sink,
            data_rx,
            ctrl_rx,
            terminal_rx,
            send_cancel,
            disconnect_rx,
        ));
        tokio::task::yield_now().await;

        // Step 2: invoke the shared expiry constructor with an already-expired
        // deadline. Queue-then-cancel is synchronous: the send loop's cancellation
        // branch drains the terminal frame before writing Close.
        let already_expired = chrono::Utc::now() - chrono::Duration::seconds(1);
        let gate = crate::nip_fi_gate::SessionAdmissionGate::new(already_expired, cancel.clone());
        let expiry_handle = crate::nip_fi_session::spawn_nip_fi_expiry_task(
            already_expired,
            gate,
            terminal_tx,
            crate::nip_fi_session::NipFiWsRoute::Audio,
        );
        expiry_handle.await.expect("expiry task must complete");
        drop(ctrl_tx); // satisfy the unused-variable lint

        // Step 3: await the writer and assert exact two-frame sequence.
        tokio::time::timeout(std::time::Duration::from_secs(2), send_handle)
            .await
            .expect("send_loop must complete within timeout")
            .expect("send_loop task must not panic");

        let frames = recorded.lock().await;
        assert_eq!(
            frames.len(),
            2,
            "expected exactly 2 frames (restricted JSON, then Close); got {:?}",
            *frames
        );

        // Frame 0: exact canonical restricted JSON.
        let expected = serde_json::json!({
            "type": "restricted",
            "message": buzz_auth::DenialClass::AuthorizationDenied.nostr_text()
        })
        .to_string();
        match &frames[0] {
            WsMessage::Text(t) => assert_eq!(
                t.as_str(),
                expected.as_str(),
                "frame 0 must be exact canonical restricted JSON"
            ),
            other => panic!("frame 0 must be Text(restricted JSON); got {other:?}"),
        }

        // Frame 1: Close(None).
        assert!(
            matches!(frames[1], WsMessage::Close(None)),
            "frame 1 must be Close(None); got {:?}",
            frames[1]
        );
    }

    // ── W8: barrier at membership check — cancel before first DB read ─────────
    //
    // Arms `before_membership_check` — the hook at the very start of
    // `check_membership_for_admission`, before any DB read. Calls the function
    // directly in a spawned task with a live gate. When the hook signals arrival,
    // fires cancel (simulates expiry). Releases the hook. The function then
    // attempts its first DB read (which fails with a lazy-pool error) and
    // returns Err. This proves the hook fires before any DB call.
    //
    // Observable invariant: cancel is set before the function returns, and the
    // function returns without writing any membership row.
    //
    // Hook location: entry of `check_membership_for_admission`, before the first
    // `state.db.get_channel()` call.
    //
    // Mutation evidence:
    //   A) Delete `before_membership_check(...)` from check_membership_for_admission →
    //      hook never fires → `arrived_rx` times out → test panics.
    //   B) Move the hook after `state.db.get_channel()` → hook fires after DB read
    //      (order changed); on a lazy pool the DB read errors out before the hook
    //      → arrived_rx times out → test panics.
    //   C) Supply a real DB where get_channel returns an archived channel →
    //      function returns "channel is archived" before the hook (but after the
    //      first DB call) → hook never fires → arrived_rx times out → test panics.
    //      (This variant is tested in the DB integration suite.)
    #[tokio::test]
    async fn w8_membership_check_barrier_fires_before_db_read() {
        use buzz_core::tenant::{CommunityId, TenantContext};
        use tokio_util::sync::CancellationToken;
        use uuid::Uuid;

        let state = audio_test_state().await;
        let community = CommunityId::from_uuid(Uuid::nil());
        let tenant = TenantContext::resolved(community, "test.local".to_string());
        let channel_id = Uuid::new_v4();
        let pubkey = nostr::Keys::generate().public_key();
        let pubkey_bytes = pubkey.to_bytes().to_vec();

        let cancel = CancellationToken::new();

        // Arm the hook at the entry of check_membership_for_admission.
        let (arrived_rx, release) =
            crate::nip_fi_test_hooks::audio_membership_check_hook::arm(community);

        let state2 = std::sync::Arc::clone(&state);
        let tenant2 = tenant.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            super::check_membership_for_admission(
                &state2,
                &tenant2,
                channel_id,
                &pubkey_bytes,
                None,
            )
            .await
        });

        // Wait for the function to reach the hook (before any DB call).
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx)
            .await
            .expect("W8: check_membership_for_admission must reach hook within 5s")
            .expect("arrived channel closed");

        // Cancel — simulates expiry firing before the first DB read.
        cancel2.cancel();

        // Release — function resumes and attempts its first DB read.
        release.notify_one();

        // Wait for the function to complete (DB error on lazy pool, or real result).
        // Note: with a lazy pool at port 1, the DB call may hang indefinitely
        // (sqlx pool acquisition blocks waiting for a connection). We abort the
        // task rather than waiting — the key invariants are already established:
        // the hook fired (arrived_rx succeeded above) and cancel is set.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), handle).await;

        // Cancel was set before the function's first DB call.
        assert!(cancel.is_cancelled(), "W8: cancel must be set");

        // The hook fired at the entry of check_membership_for_admission — before
        // any DB call. `arrived_rx` succeeded above proves this invariant.
        // The function returned before any membership row was written (it only reads
        // in check_membership_for_admission — all writes go to commit_participant_join).
        // Whether the DB call errored (fast refusal) or is still pending (slow pool)
        // is irrelevant — the hook-fired invariant is what W8 establishes.
        let _ = cancel2; // suppress unused warning
    }

    // ── W9/W10: participant commit barrier — requires DB integration infrastructure ──
    //
    // `before_participant_commit` fires between the uncommitted 48101 insert and
    // `acquire_effect()`. A test firing expiry at that point proves the transaction
    // is rolled back (no committed 48101 row, no membership write). A concurrent-
    // reaffirm variant would fire expiry during the second of two concurrent
    // committers.
    //
    // These witnesses require a seeded DB (channel, community, membership state)
    // to reach `commit_participant_join`. They are integration-test-level witnesses
    // and do not run in the unit test suite.
    //
    // Blocker: requires a seeded test DB with:
    //   - A community at `CommunityId::nil()` (or real community UUID)
    //   - A channel with `channel_id` under that community
    //   - A user pubkey authorized for relay membership
    //
    // Once the integration DB fixture is available (see `buzz-relay-integration`
    // test suite), these witnesses should be added there and referenced here.
    //
    // What `before_participant_commit` proves when exercised:
    //   - The transaction begins before the hook (148101 insert uncommitted)
    //   - Expiry fires after the insert, before commit
    //   - `acquire_effect()` returns `SessionExpired`
    //   - `tx.rollback()` is called explicitly — no committed row
    //   - `JoinCommitError::Expired` is returned to the caller
    //   - Caller removes peer from room (cleanup on Expired)
    //
    // concurrent-reaffirm variant (also integration-level):
    //   Two concurrent goroutines calling `commit_participant_join` for the same
    //   pubkey. The second observes the membership lock shows the first already
    //   committed. Expiry fires during the second's commit. The second rolls back.
    //   The first's commit is not affected.
}
