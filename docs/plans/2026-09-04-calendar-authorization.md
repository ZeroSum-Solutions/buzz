# Calendar authorization contract (T11)

Date: 2026-09-04. Owner ticket: T11 `docs/calendar-authz` in
[`2026-09-04-zs-implementation-plan.md`](2026-09-04-zs-implementation-plan.md). This memo is the
authorization contract that T12 (`feat/google-calendar`) implements and that T12a's view design
assumes. It decides nine questions; each decision below states what we do and why.

## Summary

Buzz shows the business calendar that already lives in Google Workspace. It does not own the
calendar, does not grant access to it, and does not reconcile Google's ACL with Buzz channel
membership. Each signed-in human connects their own Google account through an installed-app
OAuth flow; the desktop holds that grant in the OS keychain and renders exactly what that
account's own API calls return. Buzz membership decides where the surface appears; Google
decides what it contains. Agents get no calendar credential in v1.

This is the shape upstream RFC #3227 argues for: an outside service reaches Buzz as a
*scoped integration* with a narrow credential that never enters the agent sandbox, not as a
platform primitive. `VISION.md:9` argues against stitching outside services into the workspace;
the scoped-integration shape is what keeps that argument intact — the calendar is a view onto
someone else's system, not a Buzz data model.

## Why the two obvious alternatives are out

- **A native shared-calendar event kind.** Idiomatic Buzz (`AGENTS.md` "Prefer Nostr events over
  new HTTP endpoints"), but the relay rejects unknown kinds: `required_scope_for_kind` in
  `crates/buzz-relay/src/handlers/ingest.rs:437` returns `Err` for any kind it does not know, and
  we do not operate the hosted relay's allow-list. A native kind is an upstream-first ticket, not
  this fork's v1.
- **An embedded Google Calendar iframe.** The desktop CSP is `default-src 'self'` with no
  `frame-src` (`desktop/src-tauri/tauri.conf.json:39`), and Google Calendar refuses framing. The
  view has to be native, which means the app holds a credential, which is why this memo exists.

## Decisions

### 1. OAuth scopes requested

**Decision.** Installed-app authorization-code flow with PKCE (S256) and a loopback redirect on
`http://127.0.0.1:<ephemeral>/oauth/callback`, opened in the system browser. Two stages:

- At connect: `openid`, `https://www.googleapis.com/auth/userinfo.email`,
  `https://www.googleapis.com/auth/calendar.calendarlist.readonly`,
  `https://www.googleapis.com/auth/calendar.events.readonly`.
- At the first edit attempt, by incremental authorization (`include_granted_scopes=true`):
  `https://www.googleapis.com/auth/calendar.events`.

Never requested: `https://www.googleapis.com/auth/calendar` (full calendar management) and
`https://www.googleapis.com/auth/calendar.acl`. The granted scopes are read back from the token
response and persisted; a user who unchecks a box gets the read-only surface, not a broken write
path.

**Reason.** `calendarlist.readonly` is the only way to resolve the business calendar's id and the
caller's `accessRole` without asking for calendar management. Splitting read from write means a
member who only ever looks at the calendar never holds a token that can change it. Reading the
granted scopes back rather than assuming them is what makes the read-only fallback real: Google's
consent screen lets the user drop a scope, and an app that assumes it got what it asked for fails
at write time with a raw 403.

### 2. Which Google account binds to which Buzz identity, and how the binding is stored

**Decision.** One Google account per Buzz identity per installation. The Buzz side of the binding
is the active identity's pubkey hex (`get_identity`, `desktop/src-tauri/src/commands/identity.rs`);
the Google side is the OIDC `sub`, not the email. The record — refresh token, access token,
expiry, granted scopes, `sub`, email for display — is stored in the OS keychain through
`SecretStore` under a key namespaced by pubkey hex. `SecretStore` keeps all secrets as one JSON
blob (service `buzz-desktop`, username `secrets`, `desktop/src-tauri/src/secret_store.rs:1-21`),
so this costs no extra keychain prompt. Token exchange, refresh and every Google API call happen
in Rust. The webview receives a redacted status struct only: connected, email, granted scopes,
expiry, state. Connecting a different Google account requires an explicit confirm and revokes the
previous grant first (decision 5). The pubkey → Google-account mapping is never published to the
relay.

**Reason.** `sub` is stable; a Workspace email can be renamed or reassigned to a different human,
and a binding keyed on email would silently follow the address to the new person. Keeping tokens
out of the webview matters because the CSP's `connect-src` already allows `https:` — a token in
the renderer is one XSS away from any host. Not publishing the mapping keeps the workspace
identity graph off the relay, where the operator and every channel member would otherwise see who
holds which Google account.

### 3. Who owns the business calendar, and how sharing is granted

**Decision.** The business calendar is a Google Workspace secondary calendar owned by the
Workspace (an admin-held account or a resource account), not by any individual's primary calendar.
All sharing is granted in Google Calendar's own ACL — "Make changes to events" for staff who
schedule, "See all event details" for everyone else. Buzz never creates, grants, changes or
revokes a calendar ACL, and holds no scope that would let it. Edit affordances in the UI derive
from the `accessRole` that `calendarList` returns for that user (`owner`/`writer` enable edit;
`reader`/`freeBusyReader` disable it), and a write Google rejects surfaces as a failure — never as
a local "saved" state.

**Reason.** Two access-control systems that can disagree is the failure mode the feature audit
named (`2026-09-04-zs-feature-audit.md:57`). Buzz's only gate is channel membership
(`VISION.md:37`); Google's is the calendar ACL. Reconciling them means one of them is a stale copy
of the other, and the copy will be wrong the day someone is offboarded. Instead Buzz makes no
access decision at all: it renders each account's own answer. Removing someone from the business
means removing them in Google Workspace, which is the same sentence the audit already wrote.
Owning the calendar with the Workspace rather than a person means an offboarded owner does not
take the calendar with them.

### 4. Which channels show the calendar, and who chooses

**Decision.** The calendar surface is opt-in per channel. A channel admin (kind:39001, with
membership at kind:39002 — `crates/buzz-core/src/kind.rs:424-426`) chooses which calendar id a
channel maps to. For v1 that mapping is a local per-identity desktop setting in the app-data dir
next to the archive DB; a relay-synced mapping needs a new event kind and is deferred to an
upstream-first ticket (see "Why the two obvious alternatives are out"). The mapping is a
*display* choice and carries no authority: a channel member whose Google account is not on the
calendar's ACL sees an empty surface with "not shared with your account", never someone else's
events.

**Reason.** Making the mapping powerless is what lets it be stored loosely. If a wrong or stale
mapping could expose event data, it would need the same durability and audit as the ACL itself;
because every read is made with the viewer's own token, the worst outcome of a bad mapping is an
empty panel in the wrong channel. That trade buys v1 out of a relay change we cannot make.

### 5. Disconnect behavior

**Decision.** "Disconnect" in Buzz is one user action with an ordered, resumable effect:

1. Write the binding record to `revoke_pending` (one persist, not three).
2. Call Google's revocation endpoint with the refresh token.
3. Delete the keychain record (`SecretStore::delete`), drop the in-memory token cache, and fence
   any in-flight refresh by generation so a late response cannot rewrite a deleted record.
4. Purge the cached events for that binding (decision 6).
5. Leave the channel mapping in place, so reconnecting returns to the same view.

If step 2 fails (offline, 5xx), steps 3–5 still run and the `revoke_pending` record survives as a
durable retry journal, retried on next launch until Google confirms; it is never dropped on a
caught error. Full sign-out already covers both halves: the boot reset renames the app-data dir
and calls `delete_all_with_legacy()` then `verify_fully_wiped()` on the keychain
(`desktop/src-tauri/src/reset.rs:273,315`).

**Reason.** `AGENTS.md` Review-Proven Rule 1 — a caught failure leaves a durable retry record or
propagates; deleting the journal before the retry succeeds is exactly the PR #6269 defect. Rule 5
— one user action is one atomic persist, ordered so every prefix is consistent: a crash after step
3 leaves a revoked-or-pending grant and no local token, which is safe. Rule 2 — the generation
fence stops a completing refresh from resurrecting a deleted binding.

### 6. Cached event data on disconnect and on membership loss

**Decision.** Cached events are a bounded render cache: per binding, keyed by Google `sub` and
calendar id, in its own SQLite file in the nest — not mixed into the relay archive tables, whose
rows carry a relay access proof that calendar rows do not have
(`desktop/src-tauri/src/archive/mod.rs:1-19`). The cache holds only the expansion window the view
needs (window and recurrence expansion are T12a's to size) under a hard row cap, and every row
carries the timestamp of the refresh that produced it.

- **On disconnect:** purged, as part of decision 5.
- **On loss of Buzz channel membership:** the channel's mapping row is dropped and the surface
  disappears from that channel; the event cache is *not* purged. The user's Google access did not
  change.
- **On loss of Google access** (403/404 on the calendar, or it stops appearing in
  `calendarList`): the calendar's cached rows are purged on that response and the surface shows
  "no longer shared with your account".
- **When we cannot tell** (network failure, refresh failing): the last-good view is shown marked
  stale with its refresh time, edits are disabled, and at a 24-hour staleness ceiling the events
  are dropped and the surface asks for a reconnect.

**Reason.** Purging on Buzz membership loss would mean Buzz is enforcing Google's ACL, which
decision 3 refuses; the user still has the calendar in Google, and their local copy is theirs. The
staleness ceiling is the other half of that: an unbounded offline cache would keep showing a
calendar the user may have lost, with no bound on how long. Disabling edits from stale state stops
a write built on data we already know may be wrong. Keeping the cache out of the archive DB keeps
its access-proof invariant honest.

### 7. Revocation propagation timing

**Decision.** Propagation is poll-bound, and the bound is stated rather than promised as instant.
Access tokens are short-lived (about an hour); the surface refreshes on channel focus and, while
visible, on a bounded poll with backoff — target at most five minutes between refreshes. A
principal removed in Google Workspace loses the view at the first API call after Google applies
the change: worst case one poll interval while the surface is open, or at next open. The 24-hour
staleness ceiling from decision 6 is the outer bound for an app that cannot reach Google at all.
Google push notifications are not used in v1: they need a public HTTPS callback, and the relay's
HTTP surface is deliberately narrow (`AGENTS.md` "Nostr-first HTTP surface"). A revocation — by an
admin, or by the user at their Google account page — invalidates the refresh token, and the next
refresh returns `invalid_grant`, which is decision 8.

**Reason.** The honest statement of a poll-based system is its interval, not "immediately". Naming
the worst case makes it reviewable; a webhook would shorten it but costs a public endpoint the
fork does not want, so the trade is written down instead of hidden. The backoff and the terminal
state are Rule 4: a persistent failure must not amplify into an unbounded refresh loop.

### 8. Refresh-token failure UX

**Decision.** Two classes, handled differently.

- **Transient** (network, 5xx, 429): exponential backoff with a cap and a terminal state, no
  prompt. The view stays visible marked stale (decision 6) until the staleness ceiling.
- **Terminal** (`invalid_grant`, `invalid_client`, revoked or expired grant, a required scope no
  longer granted): the refresh loop stops at once, the binding moves to `needs_reconnect`, event
  data is dropped at the ceiling, and a quiet, persistent "Reconnect Google Calendar" action
  appears on the calendar surface *and* in settings.

The reconnect action is never hidden behind the same state it repairs, and the failure never signs
the user out of Buzz, never deletes the channel mapping, and never shows a raw OAuth error string.
Failures are logged with the reason and never with a token or an authorization code; T12 asserts
that with a test. Reconnect reuses the connect flow and keeps the binding when `sub` matches; a
different `sub` is an account change and takes the explicit confirm from decision 2.

**Reason.** Rule 6 — a guard that hides the only recovery affordance is a functional failure, so
the reconnect entry lives in two places, one of which does not depend on the broken surface
rendering. Rule 4 — a terminal auth error must stop the loop, not retry forever against a grant
that will never come back. Splitting transient from terminal is what stops a flaky network from
nagging the user to re-consent.

### 9. What an agent may read or write

**Decision.** Nothing, in v1. No managed agent, ACP harness or MCP server receives a Google
credential, and the desktop registers no calendar command on any agent-facing surface. No calendar
key is ever written into a harness environment; the identity keys already reserved from user
override (`desktop/src-tauri/src/managed_agents/reserved_env_keys.rs`) are the precedent, and a
Google refresh token is the same class of secret with a larger blast radius. T12 binds this with a
falsifiable test: the agent-facing command list and the spawn environment contain no calendar
entry, so deleting the guard fails a test.

The sanctioned way to give an agent calendar access later is RFC #3227's shape, and it is a
separate ticket: a *distinct Google principal* — a dedicated Workspace account or service account
with its own row in the calendar's ACL — delivered as a scoped credential that is substituted at
egress, never present in the owner-review draft and never inside the agent sandbox. Humans keep
their own grants; the agent gets its own, revocable on its own.

**Reason.** A per-human OAuth grant handed to an agent lets the agent act as that human inside
Google, outside Buzz's channel gate and outside anything a Workspace admin would expect to see in
an audit log. It also breaks the one-way property this contract rests on: Buzz makes no access
decisions. An agent with its own principal keeps Google authoritative — the admin can see it,
scope it, and remove it in the same place they manage everyone else.

## Deferred, with the ticket that owns it

- Event model, expansion window, month and agenda rendering, keyboard and screen-reader
  semantics — T12a `docs/calendar-view-design`.
- Implementation of this contract, the mock Google server and the live two-account checklist —
  T12 `feat/google-calendar`.
- Relay-synced channel → calendar mapping (needs a new allow-listed kind) — upstream-first,
  not scheduled.
- Agent calendar access through a separate Google principal — blocked on RFC #3227 landing
  upstream.

## Relates to

- Upstream RFC #3227 — app-integration agents with scoped credentials (the shape this follows).
- Upstream PR #1382 — the closed Google Calendar work T12 revives for the OAuth and storage half.
- `2026-09-04-zs-feature-audit.md` §4 — the audit that ruled out a native kind, Cal.com and
  iframes.
