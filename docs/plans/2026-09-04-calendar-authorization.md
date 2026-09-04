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

**Driving scenario.** Broken English (the client in the plan's R1) keeps its teaching schedule in a
Google Workspace calendar. A teacher opens the school's Buzz channel and expects this week's
classes, and expects to move one when a class is rescheduled. Someone in the same channel who is
not on that calendar's ACL must see nothing. When the school offboards a teacher in Google
Workspace on a Friday, that teacher's view has to go away without anyone touching Buzz. The
decisions below are scored against that scenario.

The contract assumes every member who should see the calendar has an account in the Workspace that
owns it. Decision 1 states what that assumption costs and what happens when it is false.

`VISION.md:9` argues against stitching outside services into the workspace. Keeping the calendar a
view onto someone else's system rather than a Buzz data model is what keeps that argument intact.

Upstream RFC #3227 is narrower than this memo and only decision 9 follows it: it asks for an
extension point so an installed app can supply a managed agent carrying its own scoped credential
that never enters the agent sandbox. It says nothing about a human authorizing a third-party API
from the desktop, which is what decisions 1–8 are; those stand on their own arguments below.

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
`http://127.0.0.1:<ephemeral>/oauth/callback`, opened in the system browser. **One authorization
request, not two.** Connect asks for the complete set the surface can ever need:

- `openid`
- `https://www.googleapis.com/auth/userinfo.email`
- `https://www.googleapis.com/auth/calendar.calendarlist.readonly`
- `https://www.googleapis.com/auth/calendar.events.readonly`
- `https://www.googleapis.com/auth/calendar.events`

Never requested: `https://www.googleapis.com/auth/calendar` (full calendar management) and
`https://www.googleapis.com/auth/calendar.acl`.

**The granted set is read back from the token response's `scope` string, and it is the only thing
the surface trusts.** Google's granular consent lets a user grant part of what was asked, so the
read-only surface is produced by the readback, not by a smaller request:

- the read surface needs `calendar.calendarlist.readonly` **and** at least one of
  `calendar.events.readonly` / `calendar.events`;
- edit affordances need `calendar.events` **and** decision 3's `accessRole` test;
- anything short of the read set is a *connect failure*, not a degraded surface: the credential is
  not persisted, the new grant is discarded by the single operation "When connect fails" defines,
  and the user is told which permission the calendar view requires.

**No incremental authorization.** Google's OAuth 2.0 for Mobile & Desktop Apps guide states that
incremental authorization is not supported for installed apps or devices, and this decision pins
the client to type Desktop app, so `include_granted_scopes=true` is never sent. When the granted
set has to change later — the user unchecked write at consent and now wants to schedule — Buzz
runs a **complete new authorization for the whole union above**, and replaces the stored credential
only after the new token response's `scope` string is verified to contain every scope the current
surface depends on.

**A grant is one per (Cloud project, Google account), not one per token.** That is the shape
Google's revocation endpoint works on: revoking any token removes every scope the account granted
to the *project* and invalidates the tokens of every client registered under it. Two token
responses for the same account are two views of one grant, not two grants, so "revoke the token we
just issued" and "revoke the credential we already hold" are the same operation. The short-scope
branch of a re-authorization is therefore decided by `sub` before anything is revoked:

- **Same `sub` as the stored binding** — the ordinary case, a user upgrading their own connection.
  The new token is dropped in memory and **never sent to the revocation endpoint**. Buzz
  re-validates the existing credential with one `calendarList` call, reports the scopes that
  credential actually holds, and says the extra permission was not granted. Google's grants are
  additive, so a consent screen on which the user granted less does not withdraw what the account
  already granted; if the stored credential has stopped working anyway, that is decision 8's
  terminal path and nothing here special-cases it.
- **Different `sub`** — not a re-authorization at all but decision 2's account change. It takes the
  explicit confirm, and a decline or a short scope set runs "discard the new grant" below, which
  may revoke, because that token belongs to a different account's grant.

We never revoke on a same-`sub` re-authorization. That is the guarantee, and it is narrower than
the one an earlier draft of this memo made — "a re-authorization can never leave an account holding
less than it held before" — which was false: honoring it by revoking the short token would have
taken the stored refresh token with it and stripped the read surface the teacher already had, on
every device she uses.

T12 names a test with two token responses for one grant: a stored binding for `sub` S holding the
full union, a re-authorization for S returning only `calendar.events`, and the assertions that no
request reaches the revocation endpoint, that the stored refresh token is byte-identical
afterwards, and that the surface still reports the original scopes.

**A dedicated Google Cloud project.** The OAuth client lives in a Google Cloud project that
contains nothing but this client, owned by the same Workspace that owns the business calendar
(decision 3). This is a requirement, not a preference: Google's revocation semantics are
per *project*, not per client — revoking removes every scope the user previously granted to the
project and invalidates the tokens of every client registered under it. A shared project holding a
dev client, a staging client or an internal script would make decision 5's Disconnect silently
invalidate that user's tokens for all of them. A Buzz-only project is what makes the blast radius
of Disconnect exactly "Buzz", which is what decision 5's confirm text claims it is.

**The OAuth client itself.** The client is of type **Desktop app** — the type the loopback-plus-PKCE
flow above requires. Its consent screen is published **Internal**, which limits it to that
Workspace's own accounts and needs no Google verification review. Publishing status is **In
production**, never **Testing**: a client left in Testing expires every refresh token after seven
days, which would turn decision 8's terminal `invalid_grant` from an exception into a weekly
reconnect prompt for every user, and would make decision 7's propagation bound read as working
when the grant is simply dead. Shipping this beyond one Workspace means an **External** client and
Google verification for the `calendar.events` scope — a separate ticket, not a config toggle. The
installed-app client secret ships inside the binary and is **not confidential**; it is an
identifier, not a credential.

**Who can connect.** Publishing the client Internal is also an eligibility rule: only accounts in
that Workspace can consent to it. This contract therefore assumes every Buzz member who should see
the business calendar has a Workspace account. The assumption is not free — Google's calendar ACL
accepts an address outside the Workspace (decision 3), so a member using a personal Google account
can hold real read access in Google and still be unable to consent here at all. In v1 that person
reads the calendar in Google's own web UI and gets no Buzz surface; giving them one means the
External client and the verification ticket named above. Onboarding someone for the calendar is
therefore "give them a Workspace account", not "add their address to the ACL".

**The authorization transaction.** Nothing about a callback is trusted on its own. Before the
system browser opens, the app creates one transaction record, held in memory only, containing: an
unpredictable `state` (128 bits from the OS CSPRNG), the PKCE verifier, the initiating identity's
pubkey hex, the redirect URI including the bound port, and a deadline five minutes out. At the
callback, all of it is validated in this order before any token exchange:

1. **`state` matches.** A missing, wrong or already-spent `state` is dropped: no exchange, no
   user-visible change, and the listener keeps waiting until its deadline. This is what stops
   another local process from driving an uncorrelated callback into the loopback port.
2. **The transaction is unspent and inside its deadline.** It is marked spent before the exchange,
   so a second callback replaying a valid `state` is a replay and is dropped.
3. **The active identity still matches the initiating pubkey** — checked before the exchange and
   again at persist time while holding the identity mutation lock. Identity can change under a
   live flow: `import_identity` is a live command
   (`desktop/src-tauri/src/commands/identity.rs:337`) and the codebase already serializes identity
   mutation against itself (`identity.rs:229,355,477`). A mismatch at either point aborts the
   flow: nothing is written under the new identity, and a token already issued by the exchange goes
   through "discard the new grant" below before the failure is reported.

Without step 3 the credential for pubkey A's Google account would be written under pubkey B, B's
surface would render A's calendar, and decision 2's "connecting a different Google account requires
an explicit confirm" would never fire, because from B's side that write is a first connect.

T12 names a test for each: a wrong `state`, a missing `state`, a second concurrent callback
replaying a valid `state`, a callback after the deadline, and an identity switch between the
browser opening and the callback.

**When connect fails.** Connect-time failure is its own axis, not one of decision 6's post-connect
states. Connect is a pipeline, and every stage has a failure; the list runs to the success
terminus and is exhaustive on purpose, because the dangerous states are the late ones where a
Google grant exists and we hold nothing:

0. **Preflight — before the browser opens.** The loopback listener binds its port, and
   `SecretStore` is probed with a write-and-delete of a scratch key. A bind failure or a locked or
   unavailable keychain is reported in the app while the user is still looking at it, and no
   browser opens. There is no plaintext fallback for a calendar credential, ever (decision 2).
1. **Authorized / not authorized.** Two observables here:
   - **Google redirects back with an `error` parameter** — the user pressed Cancel
     (`access_denied`). The surface returns to plain disconnected: "Google Calendar is not
     connected", Connect still offered, no dialog and no retry loop. The parameter is logged,
     never shown (decision 8).
   - **No callback arrives** before the five-minute deadline, after which the listener closes.
     This covers three causes — the browser window was closed, the account is outside the
     Workspace (an Internal client answers that in the browser with its own error page and never
     redirects), or something local blocked the loopback (a firewall, a proxy, a browser that
     refuses `http://127.0.0.1`). The app cannot tell them apart, so it does not guess: one
     "Couldn't finish connecting" state that lists those three causes in that order, names the
     Workspace domain this client accepts, and offers Try again.
2. **Exchanged / exchange failed.** A `state`-matched callback whose code exchange fails (network,
   5xx, an expired or replayed code). No token was issued, so there is nothing to revoke, but the
   user's consent already created a grant record on their account. The state is "Couldn't finish
   connecting", with Try again, and one sentence saying Buzz may appear in their Google account's
   third-party access list until they retry or remove it there. Retry is a fresh transaction, never
   a reuse of the spent one.
3. **Scope-verified / insufficient.** The readback rule above. An insufficient grant persists
   nothing and runs "discard the new grant" below.
4. **Persisted / persist failed.** The exchange succeeded, so a live grant exists on the user's
   account and we hold its refresh token. `SecretStore::store` returns `Result<(), String>`
   (`desktop/src-tauri/src/secret_store.rs:729`), and a locked or unavailable keychain is a real
   return value even after the preflight passed. On `Err` the app does not fall back to plaintext
   and does not report a bare failure: it runs "discard the new grant" and reports "Couldn't finish
   connecting" together with whatever that operation resolved to.
5. **Connected.** The binding is written and, in the *same* blob mutation, any revocation job this
   installation still has pending for the same (project, `sub`) is discarded (decision 5).

**"Discard the new grant" — one operation, named once.** Stage 3, stage 4, the identity-mismatch
abort above and a declined account-change confirm (decision 2) all end holding a token the exchange
just issued and no right to keep it. They run the same ordered steps, stated here rather than
re-derived at four call sites that can drift apart:

1. **The same-`sub` exception.** If the new token's `sub` matches a binding this installation
   already holds for this Cloud project, nothing is revoked: the token is dropped in memory and the
   operation is done. Revoking it would revoke that binding too — one grant per (project, account).
2. Otherwise post the refresh token to Google's revocation endpoint. HTTP 200, or a terminal
   `invalid_token` meaning the grant is already gone, finishes the operation.
3. On any other outcome, write a decision 5 revocation job for that (project, `sub`). Its retry
   schedule finishes what the network could not, and the user is told a revocation is pending.
4. If the journal write also fails, the failure message says in words that a Google grant for Buzz
   may still exist, and links to the user's Google account permissions page.

A grant with no local record and no durable instruction to remove it is exactly Review-Proven Rule
1's catch-with-no-durable-record, so it is the one outcome this list refuses to leave silent — and
it is refused in one place instead of once per stage.

T12 names a test per path into that operation: an exchange failure (no credential, no partial
record, no revocation job); an insufficient scope set with the revocation endpoint reachable (the
token is revoked, nothing persists); an insufficient scope set with the network down (a revocation
job survives and the next launch retries it); a persist failure after a successful exchange (the
issued token is revoked and the failure is reported); a persist failure whose revocation also fails
(a job survives and the next launch retries it); an identity switch between the exchange and the
persist (the token is revoked and nothing is written under either identity); and a declined
account-change confirm (the token is revoked and the previous binding is untouched).

None of these
states is "not shared with your account" (decisions 4 and 6): that message means a connected
account the calendar's ACL does not list, and showing it to someone who never reached the consent
screen sends them to an admin to fix an ACL that is not the problem.

**Reason.** `calendarlist.readonly` is the only way to resolve the business calendar's id and the
caller's `accessRole` without asking for calendar management. Asking for the whole union at connect
and deriving the surface from the readback is what keeps "read-only member" a real state without a
second authorization: Google will not give an installed app an incremental upgrade, so a design
that depends on one would hand a teacher who drags a class a token carrying only `calendar.events`,
and the same readback rule that protects the read-only surface would then correctly record that
`calendarlist.readonly` and `events.readonly` are gone — trying to edit would break reading.
Verifying the full union before replacing a credential is the same rule applied to the
re-authorization path. Naming the publishing status is not paperwork: it is the single setting that
decides whether decision 8's terminal branch is rare or weekly. Requiring a Buzz-only Cloud project
is not paperwork either: it is the only thing that makes Disconnect's user-facing promise true,
because Google revokes at project granularity. PKCE, not the client secret, is what binds the
authorization code to this app, so nothing here rests on that secret staying hidden — but PKCE
binds the code to the *app*, not to the *identity that asked*, which is what the transaction's
`state` and pubkey check add. Refusing to guess which of the three silent causes occurred is
deliberate — a confidently wrong message ("your account is not in the Workspace") sends a user who
merely closed the browser window to a Workspace admin.

### 2. Which Google account binds to which Buzz identity, and how the binding is stored

**Decision.** One Google account per Buzz identity per installation. The Buzz side of the binding
is the active identity's pubkey hex (`get_identity`, `desktop/src-tauri/src/commands/identity.rs`);
the Google side is the OIDC `sub`, not the email. The record — refresh token, access token,
expiry, granted scopes, `sub`, email for display — is stored in the OS keychain through
`SecretStore` under a key namespaced by pubkey hex. `SecretStore` keeps all secrets as one JSON
blob (the `BLOB_KEY` username `secrets`, `desktop/src-tauri/src/secret_store.rs:42-44`; the
service name is not a constant in that file but comes from `keyring_service()`,
`desktop/src-tauri/src/app_state_keyring.rs:9-23`, which returns `buzz-desktop` for release builds
and a `buzz-desktop-dev*` service otherwise), so this costs no extra keychain prompt. Decision 5's
revocation journal is a *second, separate key* in that same blob; it is deliberately not a field
of this record, because it has to outlive it. Token exchange, refresh and every Google API call
happen in Rust. The webview receives a redacted status struct only: connected, email, granted
scopes, expiry, state. Connecting a different Google account requires an explicit confirm and
revokes the previous grant first (decision 5). The pubkey → Google-account mapping is never
published to the relay.

**Where `sub` comes from.** The `sub` is read from the `id_token` in the token-endpoint response
Buzz receives directly from Google over TLS, and from nowhere else: never from a UserInfo call,
never from a value that passed through the webview, never from the loopback callback's query
string. Because that channel is direct and intermediary-free, Google's own OpenID Connect guidance
lets an app use the claims of a token received that way without full signature validation. Buzz
still checks that the `aud` claim equals this client id, and that check is not ceremony: it is what
stops a token minted for some other client from driving the account-change comparison and being
persisted as the binding identity. If a later ticket ever sources `sub` from anywhere but that
direct response, full ID-token validation — signature against the published JWKS, `iss`, `aud`,
`exp` — becomes required at that point, and the ticket that moves it owns that work.

**The command surface is part of this boundary.** Holding the token in Rust stops the *token* from
leaving the process; on its own it does not stop the token's *authority* from leaving, because the
renderer can still invoke the commands. So the calendar commands are constrained here:

- Every command takes an **opaque binding handle** minted in Rust — a random id valid only for the
  current active identity, the current community and the current binding generation. No command
  takes a caller-supplied calendar id, and no command enumerates calendars.
- An event is addressed by an event handle drawn from the rows Rust itself delivered for the
  current window, never by a raw Google event id supplied by the caller.
- On every call Rust re-derives from its own state, not from arguments: the active identity pubkey,
  the current community, the channel-to-calendar mapping (decision 4), and the `accessRole` from
  the last `calendarList` answer (decision 3). Any mismatch rejects the call.
- Adding a calendar to the mapping goes through a native confirmation outside the webview, showing
  the calendar summary and the Google account, so a renderer cannot widen the surface silently.

T12 names handler-level tests: a handle whose calendar is no longer in the current mapping is
rejected, and so is a request that carries a raw calendar id at all; a handle minted under a
previous binding generation is rejected; a handle minted under another identity is rejected; and a
handle minted in community A is rejected after a switch to B.

**Reason.** `sub` is stable; a Workspace email can be renamed or reassigned to a different human,
and a binding keyed on email would silently follow the address to the new person. Keeping tokens
out of the webview matters because the CSP's `connect-src` already allows `https:` — a token in
the renderer is one XSS away from any host. That same `https:` allowance is why the command
constraints above are load-bearing rather than decorative: an XSS that cannot read the refresh
token can still call a command, and a command that accepted a calendar or event id would let it
enumerate and edit every calendar the account-wide token reaches — personal calendars this contract
never discusses included — and exfiltrate the results through the same allowance. Not publishing the mapping keeps the workspace
identity graph off the relay, where the operator and every channel member would otherwise see who
holds which Google account.

### 3. Who owns the business calendar, and how sharing is granted

**Decision.** The business calendar is a Google Workspace secondary calendar owned by the
Workspace (an admin-held account or a resource account), not by any individual's primary calendar.
All sharing is granted in Google Calendar's own ACL — "Make changes to events" for staff who
schedule, "See all event details" for everyone else. Buzz never creates, grants, changes or
revokes a calendar ACL, and holds no scope that would let it. A write Google rejects surfaces as a
failure — never as a local "saved" state. Google's ACL also accepts addresses outside the
Workspace, and we do not restrict that: such a grant is real read access in Google, it simply
produces no Buzz surface, because the OAuth client is Internal (decision 1).

Edit affordances derive from the `accessRole` that `calendarList` returns for that user, and the
table is closed:

| `accessRole` | Edit affordances |
|---|---|
| `owner`, `writer` | enabled for every event on the calendar |
| `writerWithoutPrivateAccess` | enabled for the events the API returns in full; disabled for events returned as free/busy only, which render as busy blocks with no edit affordance |
| `reader`, `freeBusyReader` | disabled |
| **any other value, present or future** | **disabled — treated as read-only** |

The last row is the rule, not a placeholder. Google adds roles; an unrecognized role must never
default to allow, because the resulting edit fails at Google with a raw error, which this decision
forbids, and it must not be an implementation choice, because "hide edit" and "default allow" are
both defensible in isolation and only one of them is safe. T12 tests one case per row, including
an invented unknown role.

**Reason.** Two access-control systems that can disagree is the failure mode the feature audit
named (`2026-09-04-zs-feature-audit.md:57`). Buzz's only gate is channel membership
(`VISION.md:37`); Google's is the calendar ACL. Reconciling them means one of them is a stale copy
of the other, and the copy will be wrong the day someone is offboarded. Instead Buzz makes no
access decision at all: it renders each account's own answer. Removing someone from the business
means removing them in Google Workspace, which is the same sentence the audit already wrote.
Owning the calendar with the Workspace rather than a person means an offboarded owner does not
take the calendar with them. Closing the role table is the same discipline one level down: the
surface derives from Google's answer, including the answers Google has not invented yet.

### 4. Which channels show the calendar, and who chooses

**Decision.** The calendar surface is opt-in per channel, and **in v1 each user sets the mapping
locally**: it is a per-identity, per-community desktop setting in the app-data dir next to the
archive DB, and nothing carries one person's choice to anyone else's installation. Its key is the
tuple (pubkey hex, canonical community relay id, channel id), never the channel id alone — channel
ids are relay-scoped NIP-29 group ids, so one pubkey active on communities A and B would otherwise
match in B a row written for A and draw the calendar in the wrong community's channel. The channel admin (kind:39001,
with membership at kind:39002 — `crates/buzz-core/src/kind.rs:424-426`) decides which calendar id
the channel *should* use and conveys that choice out of band — a pinned message, the channel topic,
onboarding — so in v1 it is a convention the app does not enforce. Admin-owned mapping becomes
enforceable only with the relay-synced kind, which needs a new allow-listed event kind and is
deferred to an upstream-first ticket (see "Why the two obvious alternatives are out"). What T12 can
bind is therefore the local half: the mapping is per-identity and local, and no mapping grants
access. The mapping is a *display* choice and carries no authority — a *connected* channel member
whose Google account is not on the calendar's ACL sees an empty surface with "not shared with your
account", never someone else's events. That message is about the ACL and nothing else; a member who
could not connect in the first place gets decision 1's connect-time state instead, and a member
whose calendar simply could not be reached gets decision 6's unreachable state.

**A community switch is a removal path, and the calendar module registers for it.** `AGENTS.md`
"Community Switching" makes this a repository contract: switching remounts the React subtree but
leaves module-level singletons alive, so every community-scoped singleton needs its reset wired
into `resetCommunityState()` (`desktop/src/features/communities/useCommunityInit.ts:59`) in the
same change that introduces it — the comments there record a shipped defect from getting this
wrong. The calendar module registers its reset in that inventory, clearing the in-memory token
cache, the refresh poll timer, the pending-request map and the mapping cache. In-flight work is
fenced the way decision 5 fences a refresh: every list, refresh and edit response re-checks the
full tuple — pubkey, community, binding generation, `sub`, calendar id — before it is persisted and
again before it is delivered to the view, so a request started in community A that resolves after a
switch to B writes nothing and renders nothing (Rule 2). T12 names a test for exactly that delayed
A response landing after an A→B switch, and one for the same channel id existing in two
communities.

**Reason.** Making the mapping powerless is what lets it be stored loosely. If a wrong or stale
mapping could expose event data, it would need the same durability and audit as the ACL itself;
because every read is made with the viewer's own token, the worst outcome of a bad mapping is an
empty panel in the wrong channel. That trade buys v1 out of a relay change we cannot make, and it
is also what makes the v1 authority gap tolerable: an admin whose choice nobody's client enforces
cannot leak anything by being ignored, because the setting they would be enforcing has no power in
the first place.

### 5. Disconnect behavior

**Decision.** "Disconnect" is one user action, one durable write, and a resumable remainder. It
works on two *separate* records inside the same `SecretStore` blob, and the separation is the whole
point:

- the **binding record** of decision 2 — refresh token, access token, expiry, granted scopes,
  `sub`, email — under the pubkey-namespaced key;
- the **revocation journal** — a bounded collection of pending revocation jobs, under its own key
  in the same blob, and never touched by the code paths that delete a binding.

A journal entry is keyed by (Cloud project, Google `sub`, job id) and carries: the refresh token,
the display email, the pubkey-namespaced binding key it came from, the binding generation, the
cache-purge scope (the pubkey, community and calendar ids whose cache rows this binding produced —
decision 6's key tuple), the attempt count, the first-attempt time, the next-attempt time, and a
seven-day deadline. It carries the binding key and the purge scope precisely so a replay can finish
the *local* cleanup an interrupted disconnect started, before it touches the network. Rule 4 bounds
the collection: a second Disconnect of the same account supersedes the entry it finds rather than
adding one, so there is one live entry per Google account and at most sixteen entries; at the cap a
new Disconnect is refused with "resolve the pending Google revocations first", because dropping a
durable record is Rule 1's defect and growing without a bound is Rule 4's.

The ordered effect:

1. **One blob mutation** inserts the journal entry and removes the binding record together.
   `SecretStore` mutates its blob through a closure that receives the whole map
   (`desktop/src-tauri/src/secret_store.rs:395`), so this is one keychain write under one lock, not
   a `store` (`secret_store.rs:729`) followed by a `delete` (`secret_store.rs:901`). T12 adds the
   one method that exposes that closure to callers.
2. **Local cleanup, driven by the entry**: drop the in-memory token cache, fence any in-flight
   refresh by the recorded generation so a late response cannot rewrite a deleted record, and purge
   the cached events named by the entry's purge scope (decision 6). Every step is idempotent and is
   re-run from the entry at replay.
3. **Call Google's revocation endpoint** with the refresh token read from the entry.
4. **Remove the entry** — a second blob mutation — **only** on HTTP 200, or on a terminal
   `invalid_token` response, which means the grant is already gone.

The channel mapping is left in place throughout, so reconnecting returns to the same view.

That sequence has exactly three durable boundaries, and the state at each one is stated rather than
claimed in general:

- **Before the mutation.** Nothing has happened. The credential still works and Disconnect can be
  pressed again.
- **After the mutation.** There is no usable local credential, and there is a journal entry naming
  the binding key, the generation, the purge scope and the token to revoke. Whatever step 2 had
  finished, the replay finishes the rest before it calls the network, so no crash can leave a live
  credential or an unpurged event cache behind a Disconnect the user already pressed. The earlier
  draft's "write the tombstone, then delete the binding" ordering could leave exactly that, and its
  tombstone carried neither the binding key nor a purge scope, so the replay could not have
  finished the local half.
- **After the revocation returns 200.** The entry is removed. A crash before that removal replays
  the revocation, which answers `invalid_token`, which is also terminal — so the replay converges
  instead of looping.

Retries run at next launch and on network recovery with backoff and a capped attempt count; at the
cap, or at the seven-day deadline, the entry is **kept**, not dropped, and settings shows one line
— "Google Calendar revocation is still pending" — naming the account, with Retry and a link to the
user's Google permissions page. Deleting the record the retry depends on is exactly the PR #6269
defect.

T12 names a failpoint test at every durable boundary: a crash before the mutation, a crash
immediately after it, a crash part-way through the cache purge, a crash after the purge and before
the revocation call, a 5xx revocation that resumes across a restart, and a crash between a 200
response and the entry removal.

**A pending job never revokes a newer grant on this installation.** Disconnect while offline,
change your mind, press Connect, consent again: the connect pipeline's final stage (decision 1,
stage 5) discards any journal entry for the same (project, `sub`) in the same blob mutation that
writes the new binding. Executing that entry could not do what the user asked for anyway — the old
refresh token and the new one are one grant, so revoking the old one would kill the connection just
made and hand the user `needs_reconnect` with no visible cause. The abandonment is stated, not
silent: the old refresh token is left to expire on Google's own schedule, and settings records that
a pending revocation for that account was superseded by a reconnect. A journal entry for a
*different* `sub` is untouched and still runs.

Across two installations we cannot do this, and we do not pretend to: laptop A's journal is in
laptop A's keychain, so if desktop B reconnects the same account first, laptop A's replay revokes
the grant B is using. That is the cross-device effect the confirm text below already names,
arriving later than the user expected. B does not show a bare auth error for it: the
`needs_reconnect` message names the two things it can honestly be — a disconnect from another
device, or the user removing Buzz at their Google account page — and does not guess between them.

T12 names a test for disconnect-offline, then reconnect, then restart: no request reaches the
revocation endpoint, the new binding survives, and the superseded entry is gone. It names the
two-account variant too: a pending job for account A still runs after a reconnect of account B.

**Sign-out.** Full sign-out is a wipe, not a disconnect, and it needs a stated policy rather than
an accident: the boot reset renames the app-data dir and calls `delete_all_with_legacy()` then
`verify_fully_wiped()` on the keychain (`desktop/src-tauri/src/reset.rs:273,315`), which removes
the whole blob — every journal entry with it, and every token a revocation would need. The wipe is
the one path with no second chance, and one revocation attempt does not cover it: an installation
holds a binding *per identity*, and importing another identity is a live command
(`desktop/src-tauri/src/commands/identity.rs:337`), so two pubkeys each connected to their own
Google account is an ordinary state, not an exotic one. Sign-out therefore **enumerates every
binding and every pending journal entry in the blob, across every identity**, and attempts each
one under **one total deadline** of a few seconds — one budget for the whole set, no retry loop,
and no per-account budget that a slow first account can spend. Whatever is unresolved when that
deadline expires is named: the confirm lists each account by email — "these Google Calendar grants
could not be revoked and stay active until you remove Buzz at your Google account page" — with the
link, and the wipe proceeds anyway, because a sign-out that refuses to sign out is worse than a
disclosed live grant. T12 names a test with two identities, two Google accounts and overlapping
failures across a restart: both disconnects fail offline, both entries survive the restart and both
retry; then a sign-out whose first revocation succeeds and whose second fails names exactly the
second account in the confirm and still wipes.

**This is not a per-device action, and its blast radius is the Cloud project.** Google's
revocation endpoint removes every scope the user previously granted to the OAuth client's *Cloud
project* and invalidates the tokens of every client registered under that project — not one
machine's copy, and not only the client that called it. Decision 1 requires a Buzz-only project
precisely so that "every client under the project" means Buzz and nothing else; without that
requirement, a teacher pressing Disconnect could invalidate their tokens for a sibling client the
confirm text never mentioned. Within Buzz, an identity's pubkey can be live on more than one
installation (decision 2 binds one Google account per identity *per installation*), so
disconnecting on the laptop ends the desktop's grant too, and the second installation finds out
only as an `invalid_grant` on its next refresh (decision 8). We accept that rather than engineer
around it, but we do not let it arrive unexplained: the confirm reads "this disconnects Google
Calendar for Buzz on all your devices", and the resulting `needs_reconnect` elsewhere names the
cause instead of showing a bare auth error.

**Reason.** `AGENTS.md` Review-Proven Rule 1 — a caught failure leaves a durable retry record or
propagates; a journal stored *inside* the record the operation deletes is not a journal, which is
why the journal is its own key with its own deletion rule. Rule 5 — one user action is one atomic
persist, taken literally rather than approximated: the two writes that used to be steps 1 and 2 are
one blob mutation, so the torn state between them — a usable credential, an unpurged cache, and a
journal entry that would revoke the grant at the next launch — has no prefix to occur in. Rule 4 —
the journal is a capped collection with a deadline and a stated behavior at the cap, not an
unbounded queue. Rule 2 — the generation fence stops a completing refresh from resurrecting a
deleted binding.
Per-device revocation would need a distinct OAuth client or a distinct Google account per machine;
both are worse than the cross-device effect, and a "disconnect" that quietly leaves a live grant
on a machine the user no longer has is the worst option of the three.

### 6. Cached event data on disconnect and on membership loss

**Decision.** Cached events are a bounded render cache in its own SQLite file in the nest — not
mixed into the relay archive tables, whose rows carry a relay access proof that calendar rows do
not have (`desktop/src-tauri/src/archive/mod.rs:1-19`). A row's key is the full tuple (pubkey hex,
canonical community relay id, binding generation, Google `sub`, calendar id, event id), for
decision 4's reason: identity, community and binding generation all change under a running app, and
a key missing any of them lets a response written for one of them be read back under another. That
tuple is re-checked before every persist and again before every delivery to the view. The cache
holds only the expansion window the view needs (window and recurrence expansion are T12a's to size)
under a hard row cap, and every row carries the timestamp of the refresh that produced it.

- **On disconnect:** purged, as part of decision 5.
- **On loss of Buzz channel membership:** the channel's mapping row is dropped and the surface
  disappears from that channel; the event cache is *not* purged. The user's Google access did not
  change.
- **On loss of Google access:** only a *verified read failure on the calendar resource itself*
  purges, classified by the table below.
- **When we cannot tell** (network failure, refresh failing): the last-good view is shown marked
  stale with its refresh time, edits are disabled, and at a 24-hour staleness ceiling the events
  are dropped and the binding enters the **`unreachable`** state — Retry plus a line about the
  network, *not* decision 8's `needs_reconnect`.

**Classifying a failure.** A 403 is never classified on its status alone. The decision is a
function of the HTTP status, the structured `reason` in the error body, and the operation that
produced it:

| Response | Class | Effect |
|---|---|---|
| 403 `userRateLimitExceeded`, `rateLimitExceeded`, `quotaExceeded` | transient (decision 8) | keep the cache, back off; identical handling to 429 |
| 403 `forbiddenForNonOrganizer` | write-authorization | fails that write only; the read path and the cache are untouched |
| 403 `insufficientPermissions` on a write | write-authorization | edit affordances drop to read-only; if the scope readback shows `calendar.events` absent, decision 1's re-authorization is offered |
| 404 on an **event-level** request (get, patch or delete of one event id) | missing event | drop that one cached row and refresh the window; never access loss, never a message about sharing |
| 403 with an access reason, or 404, on a **calendar-level** read (`calendars.get`, `calendarList.get`, or a list of that calendar's events), still failing after one backed-off retry | access loss | purge that calendar's cached rows; surface "no longer shared with your account" |
| 401 on a resource call | expiry until proven otherwise | one generation-fenced forced refresh and one replay of that call; the refresh response classifies, never the 401 |
| refresh returning `invalid_grant`, or a 401 on the replay after a refresh that succeeded | terminal auth (decision 8) | `needs_reconnect` |
| refresh returning `invalid_client` | app error (decision 8) | `app_error`; no reconnect affordance, because a reconnect cannot repair it |
| network failure, 5xx, 429 | transient (decision 8) | keep the cache, back off |

**The matrix closes on the operation, not only the status.** Google documents 404 for two
different things: a resource that never existed, and a calendar the user cannot access. An event
someone deleted in Google answers 404 to a read of its cached id, and answers 404 again to the
backed-off retry, because a deleted event stays deleted. Classified on status alone that satisfies
the access-loss condition, purges the whole calendar, and sends a teacher to an administrator to
hunt an ACL that is correct — the support call decision 6 already removed from the rate-limit row,
one row further down. So only a **calendar-level** request can establish access loss; an
event-level 404 removes that event and nothing else.

**A 401 is an expiry until a refresh says otherwise.** Access tokens last about an hour and
decision 7's poll runs every five minutes, so a token expiring mid-call is ordinary operation, not
an authorization event. A 401 on a resource call therefore triggers exactly one forced refresh —
fenced by the binding generation, and single-flighted so a burst of concurrent 401s produces one
refresh and not one each — and then one replay of the call. Only the refresh response can be
terminal, which is Google's own instruction for this status: get a new access token with the
refresh token, and send the user through the OAuth flow only if that fails. A 401 on the replay
after a refresh that succeeded is terminal, because nothing further can repair it.

An unrecognized `reason` on a read stays transient however often it repeats. It never ages into
access loss; it ages into `unreachable` at the staleness ceiling, like any other condition we
cannot classify. Erring toward transient is deliberate and asymmetric: a wrong transient call costs
one stale poll interval, while a wrong access-loss call purges a cache and sends a teacher to an
administrator to hunt an ACL that was never wrong.

T12 names a test per row, and three more that the rows alone would not force: a deleted event that
answers 404 twice (only that row disappears, no purge and no sharing message), an unknown 403
reason that repeats until the staleness ceiling (`unreachable`, never "no longer shared"), and the
expiry-mid-call race — one 401, one refresh, one successful replay, no state change and no
Reconnect prompt — plus an assertion that five concurrent 401s produce exactly one refresh.

**Absence from `calendarList` is not evidence of anything.** The list call hides calendars for two
ordinary reasons: `showHidden` defaults to false, and `maxResults` defaults to 100 entries with
`nextPageToken` paging. T12's list call therefore sets `showHidden=true` and pages to exhaustion
before drawing any conclusion — and even then, absence only means "do not offer this calendar in
the picker". It never purges and never produces the "no longer shared" message; only a direct,
classified failure on the calendar resource does. T12 tests a calendar the user hid in Google's own
UI and a calendar sorted past entry 100: both resolve, and neither purges.

**Reason.** Purging on Buzz membership loss would mean Buzz is enforcing Google's ACL, which
decision 3 refuses; the user still has the calendar in Google, and their local copy is theirs. The
staleness ceiling is the other half of that: an unbounded offline cache would keep showing a
calendar the user may have lost, with no bound on how long. Dropping the events at the ceiling
while calling the state `unreachable` rather than `needs_reconnect` is the point of decision 8's
split — the grant is not known to be broken, and a teacher back from a weekend with no signal must
not be handed a Reconnect button that opens a browser which cannot reach Google either. Disabling
edits from stale state stops a write built on data we already know may be wrong. Keeping the cache
out of the archive DB keeps its access-proof invariant honest.

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
state are Rule 4: a persistent failure must not amplify into an unbounded refresh loop. The poll
interval is also why decision 6 puts Google's rate and quota 403s in the transient class: a whole
staff room opening Buzz at five to nine is a load spike, not an ACL change.

### 8. Refresh-token failure UX

**Decision.** Four states, handled differently.

- **Transient** (network, 5xx, 429, and the 403 rate and quota reasons decision 6's table lists,
  which Google says to handle identically to 429): exponential backoff with a cap and a terminal
  state, no prompt. The view stays visible marked stale (decision 6) until the staleness ceiling.
- **Unreachable** (a transient condition that outlives decision 6's 24-hour ceiling): the events
  are dropped, the binding moves to `unreachable`, and the offered action is **Retry**, with one
  line about checking the network. It is deliberately not `needs_reconnect`: nothing says the
  grant is broken, and a reconnect affordance here opens a browser that cannot reach Google either.
- **Terminal** (`invalid_grant`, a revoked or expired grant, a required scope no longer granted,
  or a 401 that survives decision 6's one forced refresh): the refresh loop stops at once, the
  binding moves to `needs_reconnect`, event data is dropped, and a quiet, persistent "Reconnect
  Google Calendar" action appears on the calendar surface *and* in settings.
- **App error** (`invalid_client`, and any other response saying Buzz's own OAuth client is wrong):
  the refresh loop stops, the binding moves to `app_error`, and the state line reads "Buzz's Google
  Calendar configuration is not valid". There is **no Reconnect action and no Retry**, because
  neither repairs it — the client id, its secret or its Cloud project is wrong, and only a new
  build or a console change fixes that. The reason is logged. The data is handled as in
  `unreachable`: the last-good view stays visible marked stale with edits disabled, and is dropped
  at the 24-hour ceiling.

`needs_reconnect` is reserved for that terminal class. It is never entered from a network
condition, never from a routine token expiry, and never from an app-configuration failure.
The reconnect action is never hidden behind the same state it repairs, and no failure signs the
user out of Buzz, deletes the channel mapping, or shows a raw OAuth error string. Failures are
logged with the reason and never with a token or an authorization code; T12 asserts that with a
test. Reconnect reuses the connect flow — including decision 1's full-union request and readback —
and keeps the binding when `sub` matches, without ever revoking, per decision 1's same-`sub` rule;
a different `sub` is an account change and takes the explicit confirm from decision 2.

**Reason.** Rule 6 — a guard that hides the only recovery affordance is a functional failure, so
the reconnect entry lives in two places, one of which does not depend on the broken surface
rendering. Rule 4 — a terminal auth error must stop the loop, not retry forever against a grant
that will never come back. Splitting transient from terminal is what stops a flaky network from
nagging the user to re-consent; splitting `unreachable` out of terminal is the same argument taken
to the end, because a state name is an instruction to the user and "reconnect" is the wrong
instruction for an outage. Splitting `app_error` out is that argument once more: "reconnect" is a
false instruction for a misconfigured client, because the user can consent all day and the next
refresh fails identically. Keeping a bare 401 out of the terminal class entirely is the same
concern at the other end of the scale — an hourly token lifetime against a five-minute poll makes
the expiry race routine, and a terminal branch that fires on it would turn re-consent into normal
operation. The split also depends on the client's publishing status (decision 1).
Google's seven-day refresh-token expiry binds a client whose user type is External and whose
publishing status is Testing; decision 1's Internal choice already excludes it, so the "In
production, never Testing" rule is belt-and-braces here and load-bearing for the External client
the shipping-beyond-one-Workspace ticket would need. Left in that state, the terminal branch would
stop being an exception and become the normal weekly experience.

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
- Relay-synced channel → calendar mapping, and with it admin-owned rather than per-user mapping
  (needs a new allow-listed kind) — upstream-first, not scheduled.
- A Buzz calendar surface for members outside the Workspace that owns the client (an External
  OAuth client plus Google verification for `calendar.events`, decision 1) — a separate ticket,
  not scheduled.
- Agent calendar access through a separate Google principal — blocked on RFC #3227 landing
  upstream.

## Relates to

- Upstream RFC #3227 — app-integration agents with scoped credentials (the shape decision 9
  follows; it does not cover decisions 1–8).
- Upstream PR #1382 — the closed Google Calendar work T12 revives for the OAuth and storage half.
- `2026-09-04-zs-feature-audit.md` §4 — the audit that ruled out a native kind, Cal.com and
  iframes.
