# Calendar authorization contract (T11)

Date: 2026-09-04. T11 `docs/calendar-authz`, implemented by T12 `feat/google-calendar`, assumed by T12a. Buzz renders a Google Workspace calendar the
viewer already has and grants none itself: access is Buzz channel membership intersected with Google's ACL, checked at refresh, not at render, so a cached
paint outlives membership to decision 6's `stale_after`.

## Decisions

**1. OAuth request values and callback correlation.** Decision: installed-app PKCE code flow, loopback redirect, Buzz-only Cloud project (Internal,
Production). Scopes `openid`, `email`, `calendar.events`, which already grants read and write; no `calendar.events.readonly` or `calendarList`.
`access_type=offline`, plus `prompt=consent` when no refresh token is held; an exchange without a refresh token, or short of those scopes, writes no
binding. The listener bounds bytes, sockets and read time; one flow in flight, newest wins, and a callback whose CSPRNG `state` is missing or unequal is
rejected before exchange. A CSPRNG `nonce` must come back in the ID token. Reason: PKCE protects the code, not its origin (`calendar.rs:181,261` at
`6d4f7f796`). Tests: `rejects_mismatched_state`, `requires_nonce_echo`.

**2. Google identity binding and credential redaction.** Decision: one Google account per identity per installation, identity pubkey hex to the OIDC `sub`
of an ID token validated for signature (cached JWKS), `iss`, `aud`, `exp` and decision 1's `nonce`. Before writing, the flow re-reads the active identity
and refuses if the starting pubkey is gone: `import_identity` (`identity.rs:337`) replaces it at `identity.rs:423-425`; `dms.rs:52-55` is the precedent for
re-checking a captured scope. `SecretStore` keeps tokens, scopes, `sub`, email, `client_id` and a CSPRNG `binding_generation`; no token reaches the
webview. Access, refresh and ID tokens, authorization codes, PKCE verifiers and callback URLs are wrapped in a redacting type; boundary errors are
sanitized. Tests: `refuses_binding_after_identity_swap`; a table of signature, `iss`/`aud`/`exp`, `nonce`, scope and refresh-token failures through the
production validator; and one sentinel per credential, asserting none reaches logs, command errors, UI payloads or `Debug` output.

**3. Calendar ownership and sharing.** Decision: a secondary calendar owned by a durable Workspace role account, `buzz-calendar@<workspace-domain>`, never
a person: one owner, and it dies with the account, so offboarding needs an ownership transfer first. Naming it and having the Workspace admin verify it is
an acceptance gate T12 cannot start without. Sharing lives only in Google's ACL, which Buzz never touches; affordances come from the `accessRole` in the
mapped `events.list`, narrowed by event type and organizer, and an unrecognized role is read-only.

**4. Which channels show it; what the webview may ask.** Decision: opt-in per channel, keyed by (pubkey, normalized relay URL, channel id) — not
`Community.id`: `id` and `relayUrl` are separate fields (`types.ts:1-4`) and `relayUrl` is user-mutable (`useCommunities.tsx:54-56`), so keying on it would
let a relay repoint carry the calendar elsewhere; `channelSnapshot.ts:45-53` is the precedent. Stored per identity; the admin conveys the calendar out of
band; its id is typed into a native confirm naming account, calendar and channel. Native commands take an opaque mapping id and resolve the calendar id
themselves; none accepts a calendar id from the webview as authority. Create, edit and delete revalidate the whole tuple at call time: pubkey, relay URL,
membership, binding generation, mapped calendar, Google write authority. An offline or stale view is read-only. Reason: a renderer that could name a
calendar would spend the token. Tests invoke read, create, edit and delete directly after membership removal, not the disabled button.

**5. Disconnect and the revocation journal.** Decision: one envelope per identity holds `active_binding: Option<_>` and a map of pending revocations keyed
by generation, capacity 8. Disconnect is one mutation: it clears the binding and writes the entry (refresh token, key, generation, purge predicate,
deadline) (`secret_store.rs:395` `mutate_blob`; 601 `store_all` is overwrite-only, 901 `delete` separate); then the purge, then Google's revocation
endpoint. An entry clears only when `purge_confirmed` and `revocation_confirmed` both hold; only HTTP 200 sets the second, and anything else backs off to a
seven-day ceiling, then a terminal `revocation_unconfirmed` state settings names. While an entry for (`client_id`, `sub`) is retryable, Connect for that
`sub` is refused with a named state; the user may explicitly abandon it in settings before a new binding H is issued. Terminal entries stay in the map and
count toward the cap, cleared only by the user; a ninth is refused, Connect blocked with a named state, rather than retiring one. Reason: revocation is
project-wide, so a late 200 for G would kill H; a journal with no terminal state never converges. Tests: failed revoke, then reconnect refused; abandon,
reconnect, then a delayed 200 for G leaves H untouched, since an abandoned entry never retries.

**6. Cached event data, its bounds, and the write fences.** Decision: a SQLite render cache outside the archive DB, keyed by (pubkey, community, binding
generation, `sub`, membership epoch, calendar id, event id); `community` is the normalized relay URL, the epoch a persisted monotonic counter never reused
across restart or repoint, bumped before the purge on membership loss. Bounds count database plus WAL and temp files: per (identity, calendar) partition
5,000 rows and 16 MiB; globally 128 MiB and 64 partitions; 256 KiB a row; oldest-first eviction in a partition, LRU across them, a WAL checkpoint after
every eviction and a forced one at 32 MiB of WAL no reader may defer; one fetch 90 days ahead and 30 back, 10 pages, 8 MiB, a 30-second deadline, backoff
capped at five minutes. `stale_after` is persisted and absolute: the last successful authorization refresh plus 24 hours, never extended by failure or
restart. Purge on disconnect, on membership loss and on decision 8's terminal class; for access loss only an ACL 403 or 404 on the calendar. Every token,
journal and binding write commits inside one `SecretStore` mutation (`secret_store.rs:395`) under a transition-specific predicate, not one universal rule:
refresh requires `active_binding == Active{G}` for the captured G; disconnect requires `Active{G}` and writes `None` plus pending G; journal progress
requires pending G at its expected revision counter; initial or reconnect requires `active_binding == None`, its own OAuth-flow generation and an unchanged
identity pubkey. Cache writes stay generation- and epoch-addressed and readers select only the current tuple, so a late `events.list` writes rows nobody
reads. Reason: check-then-write loses the race the purge exists for; one universal predicate cannot advance a cleared binding's journal. One barrier-held
test per transition, each failing when its predicate is removed, plus `stale_rows_unreadable_after_restart` either side of `stale_after`.

**7. Revocation propagation timing.** Decision: poll-bound — refresh on focus, at most five minutes apart while visible, with backoff; a removed principal
keeps the view one poll interval, and transient failure and offline are bounded by `stale_after`. Reason: a webhook needs a public callback.

**8. Refresh failure states and the error matrices.** Decision: four states by HTTP status plus error reason, over three total matrices — token exchange
and refresh, `events.list`, mutations — each with a default branch. Terminal — `invalid_grant`, a withdrawn scope, a 401 surviving a forced refresh, a 404
on the mapped calendar, or a 403 reading `forbidden` or `insufficientPermissions` — purges per decision 6, offering Reconnect. Transient — network, 5xx,
429, a 403 reading `rateLimitExceeded`, `userRateLimitExceeded` or `quotaExceeded` — backs off behind a stale view; past `stale_after` it becomes
`unreachable`, drops events, offers Retry. `invalid_client` shows `app_error`, neither affordance. Refresh and list defaults fail closed at `stale_after`;
the mutation default, `forbiddenForNonOrganizer` included, rejects that command alone, changing no global state. Disconnect stays available throughout.

**9. What an agent may read or write.** Decision: nothing in v1. The credential lives in a human-only module; it and the calendar commands are absent from
the ACP, MCP and CLI registries and from every spawned agent's environment — absence is the denial seam, not a caller check a future adapter could default
to "human". Managed agents run at operator trust with an unrestricted shell, so that seam protects the credential and the commands, not rendered rows on
disk (see Risks). The guard test drives the production shell and file tools, asserting they can neither reach the credential nor invoke a calendar command,
read or write. Later access takes RFC #3227's shape: a scoped vault reference for the token, one owner approval, egress substitution, and exclusion from
the owner-review draft and the agent sandbox. Reason: a human's grant lets an agent act as that human outside Buzz's gate.

## Risks accepted

- Managed agents run at operator trust: the shell takes a caller-chosen workdir (`buzz-dev-mcp/src/shell.rs:146`), the file layer enforces no containment
  (`buzz-dev-mcp/src/paths.rs:3-7`), as `VISION_AGENT.md:57` intends. The render cache is therefore readable by any process at the user's trust level,
  agents included, like every other local Buzz store; it holds no access, refresh or ID token and no authorization code, and encrypting it would not stop a
  process that can already read the keychain.
- Revocation is Cloud-project-wide: Disconnect on one machine ends the grant everywhere; others learn at next refresh as `needs_reconnect`. A sign-out wipe
  tries every pending revocation under one deadline, then proceeds; any it misses stays live.
- The v1 mapping is local per-user state, so the admin's choice is a convention no client enforces without a relay-allow-listed kind.
- The Internal OAuth client admits only Workspace members, so a guest with real Google access gets no Buzz surface; conversely membership loss purges the
  cache while Google access is untouched, so the calendar returns at the next connect.
- Buzz gains a second authorization system beside the relay, which `VISION.md:37` does not anticipate; the fork accepts that tension.
- T12's "refresh failure surfaces a reconnect state" narrows to decision 8's terminal class; the traceability table follows all four.
