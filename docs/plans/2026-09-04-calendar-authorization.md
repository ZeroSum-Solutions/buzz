# Calendar authorization contract (T11)

Date: 2026-09-04. Owner ticket: T11 `docs/calendar-authz`; implemented by T12 `feat/google-calendar` and assumed by T12a. Buzz renders
a Google Workspace calendar the viewer already has access to: it owns no calendar and grants no access, so effective access is the
intersection of Buzz channel membership and Google's calendar ACL, enforced at each refresh and not at render — a cached paint
outlives membership to decision 6's ceiling. Upstream RFC #3227 supplies decision 9 and nothing else.

## Decisions

**1. OAuth scopes and callback correlation.** Decision: installed-app PKCE code flow, loopback redirect, Buzz-only Cloud project
(Internal, Production); scopes `openid`, `userinfo.email` and `calendar.events`, which already grants read and write — no
`calendar.events.readonly`, no `calendarList` scope or call. The exchange reads the token response's granted scopes and writes no
binding on a partial grant. The listener bounds bytes, sockets and read time and carries a CSPRNG `state`: one flow in flight, newest
wins, and a callback whose `state` is missing or unequal is rejected before any exchange. A CSPRNG `nonce` goes out with the request
and must come back in the ID token. Reason: PKCE protects the code in transit, not its origin; PR #1382 already did this
(`6d4f7f796:desktop/src-tauri/src/commands/calendar.rs:181,261`). Tests: `google_calendar_rejects_mismatched_state`,
`google_calendar_requires_nonce_echo`.

**2. Google account to Buzz identity.** Decision: one Google account per identity per installation, identity pubkey hex to the OIDC
`sub` of an ID token validated for signature (cached JWKS), `iss`, `aud`, `exp` and decision 1's `nonce`. Before the binding is
written the flow re-reads the active identity and refuses if the pubkey that began the flow is no longer active — `identity.rs:337`
can replace it mid-flow, and `dms.rs:42` is the in-repo fail-closed precedent for a captured scope re-checked at callback time.
`SecretStore` keeps tokens, scopes, `sub`, email, issuing `client_id` and a CSPRNG `binding_generation`; no token reaches the webview.
Reason: PKCE protects the code, not the JWT. Test: `google_calendar_refuses_binding_after_identity_swap`.

**3. Calendar ownership and sharing.** Decision: a secondary calendar owned by one named durable Workspace role account — such a
calendar has exactly one owner and dies with that account, so offboarding it needs an ownership transfer first — shared only in
Google's own ACL, which Buzz never reads, writes or reconciles. Affordances come from the `accessRole` in the mapped calendar's
`events.list`, narrowed by event type and organizer; an unrecognized role is read-only. Reason: a calendar role does not make every
event mutable.

**4. Which channels show it, and who chooses.** Decision: opt-in per channel, keyed by (pubkey, normalized relay URL, channel id) —
not `Community.id`, a local mutable field separate from the authoritative `relayUrl` (`types.ts:1-4`), so a relay repoint cannot carry
the calendar into a channel on another relay; `channelSnapshot.ts:45-46` is the repo precedent for authorization-sensitive persisted
keys. Stored locally per identity; the admin conveys the calendar out of band. The calendar id is typed into the native confirm,
naming account, calendar and channel. Reason: the mapping carries no authority.

**5. Disconnect behavior.** Decision: the binding and its revocation journal entry (refresh token, binding key, generation, purge
predicate, deadline) are one value under one key, so a single `store_all` writes the entry and clears the binding in one mutation —
`mutate_blob` is private (`secret_store.rs:395`), `store_all` (601) only inserts or overwrites, `delete` (901) is a separate mutation.
Then the purge, then Google's revocation endpoint. Only HTTP 200 clears the entry; anything else retries with backoff to a seven-day
ceiling and then stops in a terminal `revocation_unconfirmed` state the settings surface names. Reason: Rule 5 leaves no torn prefix;
`invalid_token` does not prove the grant is gone, and a journal with no terminal state never converges after a crash past a 200.

**6. Cached event data on disconnect and membership loss.** Decision: a SQLite render cache outside the archive DB, capped at 5,000
rows per (identity, calendar) and evicted oldest first, keyed by (pubkey, community, binding generation, `sub`, membership epoch,
calendar id, event id); membership loss bumps the epoch. Purged on disconnect, on membership loss, and on decision 8's terminal class,
`invalid_grant` included. Only a verified `events.list` failure — 403 or 404 on the mapped calendar, not a transport error — purges
for access loss; anything else stays stale to a 24-hour ceiling. One rule fences every asynchronous write: a token write, a cache
write and a view delivery each compare their captured (identity pubkey, community, binding generation, `sub`, membership epoch)
against live state at completion and drop on mismatch, so a late refresh cannot resurrect a credential and a late `events.list` cannot
repopulate the cache (`dms.rs:42` again). Reason: T12 requires the purge, and a purge no write fences is undone by a race.

**7. Revocation propagation timing.** Decision: poll-bound — refresh on channel focus and at most five minutes apart while the surface
is visible, with backoff. Healthy path: a principal removed in Google keeps the view for one poll interval at worst, or until next
open. Degraded path — a transient failure held behind a stale view per decision 8, or offline — is bounded by the 24-hour ceiling, not
by the poll interval. Reason: a webhook needs a public callback the fork does not want.

**8. Refresh-token failure UX.** Decision: transient (network, 5xx, 429, quota 403s) backs off behind a stale view; `unreachable` past
the 24-hour ceiling drops events, offering Retry; terminal (`invalid_grant`, a withdrawn scope, a 401 surviving one forced refresh)
purges per decision 6 and offers Reconnect; `invalid_client` is `app_error` with neither. Reason: Rules 6 and 4; honest state names.

**9. What an agent may read or write.** Decision: nothing in v1 — no managed agent, ACP harness or MCP server gets a Google credential
and no calendar command is agent-facing. The denial seam is the credential lookup in the calendar command layer, which refuses a
managed-agent caller; a test that fails when that guard is removed covers agent read and agent write attempts separately, as T12's
traceability table requires. Later access takes RFC #3227's whole shape: a scoped vault reference in place of the user's token, one
owner approval, substitution at egress, and exclusion from both the owner-review draft and the agent sandbox. Reason: a human's grant
lets an agent act as that human outside Buzz's gate.

## Risks accepted

- Revocation is Cloud-project-wide and cross-device: Disconnect on one machine ends the grant on every machine, and the other learns
  of it as `needs_reconnect` at its next refresh.
- A sign-out wipe attempts every pending revocation under one deadline and then proceeds; a grant it could not revoke stays live, and
  the confirm names that account.
- The v1 mapping is a local per-user setting, so the admin's choice is a convention no client enforces. Enforcing it needs a
  relay-allow-listed kind this fork cannot add.
- The Internal OAuth client admits only members of the Workspace that owns it; an outside guest can hold real read access in Google
  and still get no Buzz surface.
- Propagation is poll-bound, not immediate, and offline staleness is bounded only by the 24-hour ceiling.
- Membership loss purges the local cache while the user's Google access is unchanged, so the calendar returns intact at the next
  connect or mapping.
- Buzz gains a second authorization system beside the relay, which `VISION.md:37` does not anticipate. The fork accepts that tension
  rather than claiming the model is intact.
- T12's "refresh failure surfaces a reconnect state" is narrowed here to the terminal class; T12's traceability table follows decision
  8's four states.
- The filename resolves the plan's `2026-09-xx-calendar-authorization.md`; the plan text is left alone, because other wave-1 tickets
  share that file.
