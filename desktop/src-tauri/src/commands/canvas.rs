use tauri::State;

use crate::{
    app_state::AppState,
    events,
    relay::{query_relay, submit_event},
};

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
    expected_revision: Option<String>,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let uuid = uuid::Uuid::parse_str(&channel_id)
        .map_err(|_| format!("invalid channel UUID: {channel_id}"))?;

    // Advisory optimistic-concurrency check (client-side, two-stage). A
    // conflict-checked save asserts the revision the editor loaded. Stage one:
    // read the live head once before publishing and compare locally, returning
    // a frozen pre-write conflict marker if it already moved — this catches the
    // realistic stale-edit case (head moved minutes ago). Stage two, after
    // publishing (below): re-read the head once and confirm our write is (or is
    // built upon by) the visible head, surfacing a distinct post-write
    // supersession marker otherwise. Detection is bounded to a competitor
    // visible at check time; preventing the race entirely — a competitor that
    // lands between our read and write, or after the post-write read — needs
    // relay-side linearization (phase 2).
    //
    // `head` is `None` when the channel has no canvas yet. A matched head's
    // `created_at` is the floor for writer discipline: an accepted save stamps
    // `created_at = max(now, head + 1)` via the SDK's `canvas_write_created_at`
    // — the one home for canvas timestamp discipline — so it sorts strictly
    // ahead of the head it read under `created_at DESC, id ASC`. That helper
    // also refuses a head timestamped far in the future, so a poisoned timeline
    // fails loudly here rather than being silently extended. The no-head /
    // unconditional-append case has no floor and keeps the default `now`.
    let head = current_canvas_head(&state, &channel_id).await?;
    let prior_head_created_at = check_canvas_precondition(expected_revision.as_deref(), head)?;

    let mut builder = events::build_set_canvas(uuid, &content, expected_revision.as_deref())?;
    if let Some(floor) = prior_head_created_at {
        builder = builder.custom_created_at(nostr::Timestamp::from(
            buzz_sdk_pkg::canvas_write_created_at(floor as u64).map_err(|e| e.to_string())?,
        ));
    }
    let result = submit_event(builder, &state).await?;

    // Post-write supersession detection (only for conflict-checked writes). The
    // precondition above closes the stale-edit case; this closes the narrower
    // window where a concurrent write we could not see at precondition time has
    // become visible by now. Re-read the head once and classify: our event is
    // the head, or a later write legitimately built on it → success; anything
    // else → our revision was superseded (it is preserved in history, not
    // lost). An unconditional append (`None`) has nothing to assert, so it stays
    // fire-and-forget. This cannot close the residual race where the competitor
    // lands *after* this read — that needs relay linearization (phase 2).
    //
    // The submit above was accepted, so the write is durable. Only the
    // *verification read* can still fail here; when it does we must not present
    // an accepted publish as a failed save. Report success with `verified:
    // false` so the caller can invalidate and refetch, distinguishing an
    // unverified-but-accepted write from a detected supersession (which keeps
    // its frozen conflict marker). Supersession detection itself is unchanged.
    let mut verified = true;
    if expected_revision.is_some() {
        match current_canvas_head_ancestry(&state, &channel_id).await {
            Ok(head) => {
                let (head_id, head_expected_revision) = match &head {
                    Some((id, rev)) => (Some(id.as_str()), rev.as_deref()),
                    None => (None, None),
                };
                if !buzz_sdk_pkg::canvas_write_survived(
                    &result.event_id,
                    head_id,
                    head_expected_revision,
                ) {
                    return Err(CANVAS_SUPERSEDED.to_string());
                }
            }
            Err(_) => verified = false,
        }
    }

    Ok(serde_json::json!({
        "ok": true,
        "event_id": result.event_id,
        "verified": verified,
    }))
}

/// Frozen conflict markers the desktop TypeScript layer (`canvasConflict.ts`)
/// matches to render the "canvas changed — reload" state. The advisory check
/// in [`set_canvas`] produces these directly; keep them byte-identical to the
/// `CANVAS_CONFLICT_MARKERS` list on the TS side.
const CANVAS_CHANGED: &str = "conflict: canvas changed since it was loaded";
const CANVAS_REVISION_MISSING: &str = "conflict: canvas revision does not exist";
/// Post-write marker: the save published successfully but a concurrent write is
/// now the visible head. The user's revision is **not** lost — it is preserved
/// in history — so the TS surface renders a distinct "reload, then restore it
/// if needed" message rather than the pre-write "reapply your edit" message.
/// Keep byte-identical to `CANVAS_SUPERSEDED_MARKER` on the TS side.
const CANVAS_SUPERSEDED: &str = "conflict: canvas save was superseded by a concurrent write";

/// Pure advisory precondition: compare the revision the editor asserts against
/// the live `head` (`(event_id, created_at)` or `None` when no canvas exists),
/// returning the head `created_at` floor for writer discipline on success or a
/// frozen conflict marker on mismatch.
///
/// - `None` asserts nothing (unconditional append) — no floor.
/// - `Some("none")` asserts no canvas yet — a present head is a conflict.
/// - `Some(id)` asserts that head — a missing head is `revision does not
///   exist`, a different head is `changed since it was loaded`, a match returns
///   its `created_at` as the floor.
fn check_canvas_precondition(
    expected_revision: Option<&str>,
    head: Option<(String, i64)>,
) -> Result<Option<i64>, String> {
    match expected_revision {
        None => Ok(None),
        Some("none") => {
            if head.is_some() {
                Err(CANVAS_CHANGED.to_string())
            } else {
                Ok(None)
            }
        }
        Some(revision) => match head {
            None => Err(CANVAS_REVISION_MISSING.to_string()),
            Some((head_id, _)) if !head_id.eq_ignore_ascii_case(revision) => {
                Err(CANVAS_CHANGED.to_string())
            }
            Some((_, created_at)) => Ok(Some(created_at)),
        },
    }
}

/// Read the live canvas head as `(event_id, created_at)`, or `None` when the
/// channel has no canvas yet. The relay orders `created_at DESC, id ASC`, so a
/// `limit: 1` query returns exactly the head every surface agrees on.
async fn current_canvas_head(
    state: &AppState,
    channel_id: &str,
) -> Result<Option<(String, i64)>, String> {
    let events = query_relay(
        state,
        &[serde_json::json!({
            "kinds": [40100],
            "#h": [channel_id],
            "limit": 1
        })],
    )
    .await?;
    Ok(events
        .first()
        .map(|event| (event.id.to_hex(), event.created_at.as_secs() as i64)))
}

/// Read the live canvas head as `(event_id, expected-revision tag)` for the
/// post-write supersession check. The second element is the head's own
/// `["expected-revision", …]` tag value (the id it built on), or `None` when
/// the head carries no such tag. Returns `None` when the channel has no canvas.
async fn current_canvas_head_ancestry(
    state: &AppState,
    channel_id: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    let events = query_relay(
        state,
        &[serde_json::json!({
            "kinds": [40100],
            "#h": [channel_id],
            "limit": 1
        })],
    )
    .await?;
    Ok(events.first().map(|event| {
        let expected_revision = event
            .tags
            .iter()
            .find(|t| {
                t.as_slice()
                    .first()
                    .is_some_and(|k| k == "expected-revision")
            })
            .and_then(|t| t.as_slice().get(1).cloned());
        (event.id.to_hex(), expected_revision)
    }))
}

/// One page of a channel canvas's revision stream (kind:40100), newest first.
/// Each 40100 write is a regular signed event the relay retains, so the
/// standard query surface holds the complete history. The composite
/// `(until, before_id)` cursor mirrors the relay read order
/// (`created_at DESC, id ASC`) so paging never skips or repeats a revision when
/// several share the same second. `next_cursor` is present only when a full
/// page came back, i.e. older revisions may remain.
#[tauri::command]
pub async fn get_canvas_history(
    channel_id: String,
    limit: Option<usize>,
    until: Option<u64>,
    before_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    if before_id.is_some() && until.is_none() {
        return Err("before_id requires until".to_string());
    }
    // Bound the page size to the relay's read maximum. Beyond 1,000 the relay
    // silently clamps the returned rows, which would make `events.len() ==
    // page_size` false and null the cursor even when older revisions remain,
    // stranding them behind an unreachable page.
    let page_size = resolve_history_page_size(limit)?;

    let mut filter = serde_json::json!({
        "kinds": [40100],
        "#h": [channel_id],
        "limit": page_size,
    });
    if let Some(value) = until {
        filter["until"] = serde_json::json!(value);
    }
    if let Some(ref value) = before_id {
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("before_id must be a 64-character hex event id".to_string());
        }
        filter["before_id"] = serde_json::json!(value);
    }

    let events = query_relay(&state, &[filter]).await?;

    let revisions: Vec<serde_json::Value> = events
        .iter()
        .map(|event| {
            serde_json::json!({
                "event_id": event.id.to_hex(),
                "content": event.content,
                "created_at": event.created_at.as_secs(),
                "author": event.pubkey.to_hex(),
            })
        })
        .collect();

    // A full page means the relay may hold older revisions; hand back the
    // last event as the cursor for the next "Load older" request. A short page
    // is the tail, so there is no next cursor.
    let next_cursor = if events.len() == page_size {
        events.last().map(|last| {
            serde_json::json!({
                "created_at": last.created_at.as_secs(),
                "event_id": last.id.to_hex(),
            })
        })
    } else {
        None
    };

    Ok(serde_json::json!({
        "revisions": revisions,
        "next_cursor": next_cursor,
    }))
}

/// Resolve and validate the history page size against the relay's read
/// maximum. Defaults to 100 when unset; a value outside `1..=1000` is rejected
/// so cursor generation is never based on a size the relay would silently
/// clamp (which strands older revisions behind a falsely-terminated page).
fn resolve_history_page_size(limit: Option<usize>) -> Result<usize, String> {
    let page_size = limit.unwrap_or(100);
    if !(1..=1000).contains(&page_size) {
        return Err("limit must be between 1 and 1000".to_string());
    }
    Ok(page_size)
}

#[cfg(test)]
mod tests {
    use super::{check_canvas_precondition, resolve_history_page_size};

    const HEAD_ID: &str = "aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44";

    #[test]
    fn precondition_none_assertion_is_unconditional_append() {
        // No asserted revision: append regardless of head, no floor.
        assert_eq!(check_canvas_precondition(None, None), Ok(None));
        assert_eq!(
            check_canvas_precondition(None, Some((HEAD_ID.to_string(), 100))),
            Ok(None)
        );
    }

    #[test]
    fn precondition_expect_none_conflicts_when_a_head_exists() {
        // First-creation race: expected no canvas but one now exists.
        assert_eq!(check_canvas_precondition(Some("none"), None), Ok(None));
        assert_eq!(
            check_canvas_precondition(Some("none"), Some((HEAD_ID.to_string(), 100))),
            Err(super::CANVAS_CHANGED.to_string())
        );
    }

    #[test]
    fn precondition_expect_head_returns_floor_or_conflict() {
        // Matching head returns its created_at as the writer-discipline floor.
        assert_eq!(
            check_canvas_precondition(Some(HEAD_ID), Some((HEAD_ID.to_string(), 100))),
            Ok(Some(100))
        );
        // Case-insensitive id match still resolves.
        assert_eq!(
            check_canvas_precondition(
                Some(&HEAD_ID.to_uppercase()),
                Some((HEAD_ID.to_string(), 100))
            ),
            Ok(Some(100))
        );
        // Head moved to a different revision since load.
        assert_eq!(
            check_canvas_precondition(Some(HEAD_ID), Some(("ff".repeat(32), 100))),
            Err(super::CANVAS_CHANGED.to_string())
        );
        // Asserted a head but the canvas no longer has one.
        assert_eq!(
            check_canvas_precondition(Some(HEAD_ID), None),
            Err(super::CANVAS_REVISION_MISSING.to_string())
        );
    }

    #[test]
    fn defaults_to_100_when_unset() {
        assert_eq!(resolve_history_page_size(None).unwrap(), 100);
    }

    #[test]
    fn rejects_zero() {
        assert!(resolve_history_page_size(Some(0)).is_err());
    }

    #[test]
    fn accepts_relay_maximum() {
        assert_eq!(resolve_history_page_size(Some(1000)).unwrap(), 1000);
    }

    #[test]
    fn rejects_above_relay_maximum() {
        assert!(resolve_history_page_size(Some(1001)).is_err());
    }
}
