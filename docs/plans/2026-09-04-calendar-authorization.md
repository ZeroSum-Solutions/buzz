# Calendar authorization contract (T11)

Date: 2026-09-04. Ticket T11 `docs/calendar-authz`; implemented by T12 `feat/google-calendar`, assumed by T12a. Buzz renders a Google
Workspace calendar the viewer already has access to and grants none itself, so effective access is the intersection of Buzz channel
membership and Google's ACL, checked at refresh, not at render — a cached paint outlives membership to decision 6's ceiling.

## Decisions

**1. OAuth request values and callback correlation.** Decision: installed-app PKCE code flow, loopback redirect, Buzz-only Cloud project
(Internal, Production). Wire scopes: `openid`, `email` (or `https://www.googleapis.com/auth/userinfo.email`) and
`https://www.googleapis.com/auth/calendar.events`, which already grants read and write; no `calendar.events.readonly` or `calendarList`.
The request carries `access_type=offline`, and `prompt=consent` when no refresh token is held; an exchange returning no refresh token, or
scopes short of those three, writes no binding. The listener bounds bytes, sockets and read time and carries a CSPRNG `state`: one flow in
flight, newest wins, a callback whose `state` is missing or unequal rejected before any exchange. A CSPRNG `nonce` must come back in the
ID token. Reason: PKCE protects the code, not its origin; PR #1382 did this
(`6d4f7f796:desktop/src-tauri/src/commands/calendar.rs:181,261`). Tests: `rejects_mismatched_state` and `requires_nonce_echo`.

**2. Google account to Buzz identity.** Decision: one Google account per identity per installation, identity pubkey hex to the OIDC `sub`
of an ID token validated for signature (cached JWKS), `iss`, `aud`, `exp` and decision 1's `nonce`. Before writing the binding the flow
re-reads the active identity and refuses if the starting pubkey is gone — `import_identity` (`identity.rs:337`) replaces it at
`identity.rs:423-425`; `dms.rs:52-55` is the in-repo precedent for that re-check at callback time. `SecretStore` keeps tokens, scopes,
`sub`, email, `client_id` and a CSPRNG `binding_generation`; no token reaches the webview. Test: `refuses_binding_after_identity_swap`.

**3. Calendar ownership and sharing.** Decision: a secondary calendar owned by one durable Workspace role account named in T12 —
`buzz-calendar@<workspace-domain>`, never a person: such a calendar has one owner and dies with the account, so offboarding needs an
ownership transfer first. It is shared only in Google's ACL, which Buzz never reads, writes or reconciles. Affordances come from the
`accessRole` in the mapped calendar's `events.list`, narrowed by event type and organizer; an unrecognized role is read-only.

**4. Which channels show it, who chooses, and what the webview may ask for.** Decision: opt-in per channel, keyed by (pubkey, normalized
relay URL, channel id) — not `Community.id`, a local mutable field separate from the authoritative `relayUrl` (`types.ts:1-4`), so a relay
repoint cannot carry the calendar to another relay's channel; `channelSnapshot.ts:45-53` is the repo precedent for such keys. Stored
locally per identity; the admin conveys the calendar out of band and its id is typed into a native confirm naming account, calendar and
channel. Every native calendar command takes an opaque mapping id and resolves the calendar id natively; none accepts a calendar id plus
event id from the webview as authority. Every mutating command — create, edit, delete — revalidates the whole tuple at call time: pubkey,
normalized relay URL, channel membership, binding generation, mapped calendar, Google write authority. An offline or stale view is
read-only. Reason: the mapping carries no authority, and a renderer that could name a calendar would spend the stored token. Test:
`edit_rejected_after_membership_removal` invokes the command directly, not the disabled button.

**5. Disconnect and the revocation journal.** Decision: one envelope per identity, under one key, holds `active_binding: Option<_>` and a
bounded map of pending revocations keyed by generation, capacity 8; reconnect proceeds while entries pend, and a ninth retires the oldest
to the terminal state below. Disconnect is one `store_all`: it clears the binding and writes the entry (refresh token, key, generation,
purge predicate, deadline) in one mutation (`secret_store.rs`: `mutate_blob` private 395, `store_all` 601 overwrite-only, `delete` 901
separate). Then the purge, then Google's revocation endpoint. An entry clears only when `purge_confirmed` and `revocation_confirmed` both
hold, and only HTTP 200 sets the second; anything else backs off to a seven-day ceiling, then stops in a terminal `revocation_unconfirmed`
state the settings surface names. Reason: Rule 5 leaves no torn prefix; a journal with no terminal state never converges.

**6. Cached event data, its bounds, and the write fence.** Decision: a SQLite render cache outside the archive DB, keyed by (pubkey,
community, binding generation, `sub`, membership epoch, calendar id, event id). `community` is the normalized relay URL in every cache and
fence tuple; the epoch is a persisted, monotonic counter never reused across a restart or relay repoint, bumped before the purge on
membership loss. Bounds: per (identity, calendar) partition 5,000 rows and 16 MiB; globally 128 MiB and 64 partitions; 256 KiB per row;
oldest-first eviction in a partition, LRU across partitions; one fetch 90 days ahead and 30 back, 10 pages, 8 MiB, a 30-second deadline,
backoff capped at five minutes. Purged on disconnect, on membership loss, and on decision 8's terminal class; only its ACL 403 or 404 on
the mapped calendar purges for access loss, anything else staying stale to the 24-hour ceiling. One rule fences every asynchronous write:
a token, journal or binding write commits inside one `SecretStore` mutation, and only if the value under decision 5's envelope key still
matches `Active { generation: G, .. }` for the captured G — a compare-and-swap inside that one mutation, never a compare then `store_all`.
Cache writes stay generation- and epoch-addressed and readers select only the current tuple, so a late `events.list` writes rows no reader
reaches. Reason: check-then-write loses the race the purge exists for (`dms.rs:52-55`). Tests, each barrier-held and each failing when the
compare-and-swap is removed: `refresh_after_disconnect_does_not_restore_binding` and `events_list_after_membership_loss_is_unreadable`;
`stale_rows_unreadable_after_restart` restarts with membership lost and rows on disk.

**7. Revocation propagation timing.** Decision: poll-bound — refresh on channel focus and at most five minutes apart while the surface is
visible, with backoff. A principal removed in Google keeps the view for one poll interval at worst; a transient failure behind a stale
view, and an offline client, are bounded by the 24-hour ceiling instead. Reason: a webhook needs a public callback.

**8. Refresh failure states and the Google error matrix.** Decision: four states, chosen by HTTP status plus error reason, stated once for
the whole memo. Terminal — `invalid_grant`, a withdrawn scope, a 401 surviving one forced refresh, a 404 on the mapped calendar, or a 403
on it whose reason is `forbidden` or `insufficientPermissions` — purges per decision 6 and offers Reconnect. Transient — network, 5xx,
429, and a 403 whose reason is `rateLimitExceeded`, `userRateLimitExceeded` or `quotaExceeded` — backs off behind a stale view; past the
24-hour ceiling it becomes `unreachable`, drops events and offers Retry. `invalid_client` shows `app_error` with neither affordance, the
24-hour ceiling still applying. Disconnect stays available in every state, including `app_error`.

**9. What an agent may read or write.** Decision: nothing in v1. The Google credential lives in a human-only command module, and the
calendar commands and that credential are structurally absent from the ACP, MCP and CLI command registries and from the environment of
every spawned agent — absence is the denial seam, not a caller check a future adapter could default to "human". The guard test asserts
against those production registries and the real spawn environment, not a constructed caller enum, and covers agent read and agent write
separately, as T12's traceability table requires. Later access takes RFC #3227's whole shape: a scoped vault reference in place of the
user's token, one owner approval, substitution at egress, and exclusion from both the owner-review draft and the agent sandbox. Reason: a
human's grant lets an agent act as that human outside Buzz's gate.

## Risks accepted

- Revocation is Cloud-project-wide: Disconnect on one machine ends the grant everywhere, and others learn of it as `needs_reconnect` at
  next refresh. A sign-out wipe tries every pending revocation under one deadline, then proceeds; any it misses stays live.
- The v1 mapping is local per-user state, so the admin's choice is a convention no client enforces without a relay-allow-listed kind.
- The Internal OAuth client admits only members of the owning Workspace, so an outside guest with real Google access gets no Buzz surface;
  conversely membership loss purges the cache while Google access is untouched, so the calendar returns at the next connect.
- Buzz gains a second authorization system beside the relay, which `VISION.md:37` does not anticipate; the fork accepts that tension.
- T12's "refresh failure surfaces a reconnect state" narrows to decision 8's terminal class; the traceability table follows all four.
