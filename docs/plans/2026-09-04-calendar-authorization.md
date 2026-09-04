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

- the read surface needs **at least one of `calendar.events.readonly` / `calendar.events`**, and
  nothing else. That is what `events.list` requires, and `events.list` is the only probe this
  contract reads a calendar with (decisions 3 and 6);
- `calendar.calendarlist.readonly` is still requested, and is still verified before a credential
  *replacement*, but it is **not** part of the read set: it feeds the deferred picker and the "this
  calendar is not in your list" diagnostic, neither of which v1 ships or depends on. A user who
  unchecks it at the consent screen connects, and the surface is whole, because decision 4's
  mapping arrives through decision 2's proposal command and not through a list;
- edit affordances need `calendar.events` **and** decision 3's `accessRole` test;
- anything short of the read set is a *connect failure*, not a degraded surface: the credential is
  not persisted, the new grant is discarded by the single operation "When connect fails" defines,
  and the user is told which permission the calendar view requires;
- the scope set written to the binding record is **exactly** the readback string, parsed and stored
  verbatim. It is never widened locally to the set that was *requested*, and no code path adds a
  scope to a stored binding without a token response that justifies it.

**Why the union at connect, and what the read-only-first alternative costs.** The alternative was
considered: ask at Connect for only `openid`, `userinfo.email`, `calendar.calendarlist.readonly`
and `calendar.events.readonly`, and route write authority through a later authorization. It is the
smaller default privilege, and its cost is stated here rather than waved off. Because incremental
authorization is unavailable to installed apps (below), the upgrade is a **complete second
consent** for the whole union, so every teacher who schedules pays a full re-authorization the
first time she drags a class, on a client that cannot merge the two grants. It lost on three
counts. The teacher who schedules is the ordinary user of this surface, not the exception. Granular
consent already hands the read-only user the smaller grant without a second request — the read set
does not contain `calendar.events`, so unchecking write at the consent screen still connects. And a
second full consent is the event decision 8 works hardest to keep rare: a user who is asked for
Google permissions twice reads the second prompt as a defect, not as a privilege boundary. What the
union costs is real and is not hidden: `calendar.events` authorizes editing events on every
calendar the account can write, personal calendars this contract never discusses included, which is
exactly why decision 2's command constraints are load-bearing rather than decorative. T12 asserts
the half that holds either way: the persisted scope set equals the readback string exactly, for a
full grant and for a partial one, and no test may observe a stored scope that no token response
returned.

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
branch of a re-authorization is therefore decided by `sub`, and neither branch revokes:

- **Same `sub` as the stored binding** — the ordinary case, a user upgrading their own connection.
  The new token is dropped in memory and **never sent to the revocation endpoint**. Buzz
  re-validates the existing credential with one `events.list` call on the mapped calendar
  (decision 3's authoritative probe), reports the scopes that credential actually holds, and says
  the extra permission was not granted. Google's grants are additive, so a consent screen on which
  the user granted less does not withdraw what the account already granted; if the stored
  credential has stopped working anyway, that is decision 8's terminal path and nothing here
  special-cases it.
- **Different `sub`** — not a re-authorization at all but decision 2's account change. It takes the
  explicit confirm, and a decline or a short scope set runs "discard the new grant" below. That
  operation does not revoke either, and for the same reason one level out: the account whose token
  we are holding may hold a live Buzz grant on a device this installation knows nothing about, and
  revoking at project granularity would end it there.

**We never revoke a grant except on an action the user took against that account** — Disconnect, or
the confirmed half of an account change. That is the guarantee. It is wider than the same-`sub`
rule an earlier draft settled for, because the same-`sub` test asks the wrong question: it looks at
whether *this installation* holds a record, when what decides the blast radius is whether *that
Google account* holds a grant anywhere. It is also narrower than the guarantee the first draft
made — "a re-authorization can never leave an account holding less than it held before" — which was
false: honoring it by revoking the short token would have taken the stored refresh token with it
and stripped the read surface the teacher already had, on every device she uses.

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

**Connect is single-flight per identity.** At most one transaction record exists for an identity at
a time. Pressing Connect again — a double press, or a second channel's Connect button — supersedes
the live record rather than adding one: the old record is dropped, its listener is closed and its
port released, and only then is the new record created and the new browser tab opened. A callback
for a superseded transaction therefore arrives carrying a `state` the app no longer knows, and
step 1 drops it. Newest wins by construction, enforced by the check that is already there rather
than by a second mechanism. Without this, two flows for one identity each hold a valid distinct
`state` and each pass the pubkey check, so the *older* one, completing second, would overwrite the
account the user just chose — or, if the two flows chose different accounts, drive decision 2's
account-change path against the binding the user made seconds earlier. That is `AGENTS.md`
Review-Proven Rule 2 applied to connect, which this memo already applies to refreshes (decision 5)
and to late list responses (decision 4).

T12 names a test for each: a wrong `state`, a missing `state`, a second concurrent callback
replaying a valid `state`, a callback after the deadline, an identity switch between the browser
opening and the callback, and two Connect flows for one identity completing in reverse order —
where only the newest persists and the older's callback is dropped without a token exchange.

**Every Google request is bounded, and so is every page loop.** The five-minute deadline above
bounds waiting for the *browser*. It bounds no HTTP call, and a token endpoint that accepts the
connection and then never finishes its response would otherwise leave Connect in no state at all —
past its listener deadline, short of stage 2, with nothing to report. One shared client carries
every Google request this contract makes — token exchange, refresh, revocation, `events.list`,
`calendarList` — and its limits are stated once here instead of per call site:

- **Deadlines.** Ten seconds to establish the connection, thirty seconds without a received byte,
  and sixty seconds total for one request including redirects and body read; whichever expires
  first aborts it. An operation built from several requests — a connect, a window refresh — carries
  its own total of ninety seconds. Decision 5's sign-out budget is tighter and wins where it
  applies.
- **A response byte cap** of eight mebibytes. A body still arriving at the cap aborts the request
  rather than buffering on; no legitimate response on this surface comes near it.
- **Page bounds.** A paged read stops at twenty pages, or at the item cap for that resource — 2,500
  events for one window, 500 `calendarList` entries — whichever comes first, and treats a
  `nextPageToken` it has already seen in the same operation as a protocol error rather than a page.
  Reaching a bound is not silent: it is logged with the resource and the page count, and the window
  renders from what arrived, marked stale.
- **Cancellation.** Every request is cancellable, and is cancelled on identity change, on
  `resetCommunityState()` (decision 4), on Disconnect, and at shutdown. A cancelled request writes
  nothing and delivers nothing — the same fence decision 4 states for a late response.

An aborted, capped or page-bounded request is a **transient** failure everywhere in decision 6's
table. It is never access loss and never terminal auth: a stall proves nothing about the ACL or the
grant, and Rule 4 asks for a bound, not for a verdict. T12 binds this to a test server that accepts
a connection and then sends nothing, one that sends a body past the cap, and one that answers every
page with the same `nextPageToken`. Each must produce a transient failure inside the stated
deadline, with no purge, no state change and no unbounded loop.

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
   5xx, an expired or replayed code). No token was issued, but the user's consent already created a
   grant record on their account. The state is "Couldn't finish connecting", with Try again, and one sentence saying Buzz may appear in their Google account's
   third-party access list until they retry or remove it there. Retry is a fresh transaction, never
   a reuse of the spent one.
3. **Scope-verified / insufficient.** The readback rule above. An insufficient grant persists
   nothing and runs "discard the new grant" below.
4. **Persisted / persist failed.** The exchange succeeded, so a live grant exists on the user's
   account and we hold its refresh token. `SecretStore::store` returns `Result<(), String>`
   (`desktop/src-tauri/src/secret_store.rs:729`), and a locked or unavailable keychain is a real
   return value even after the preflight passed. On `Err` the app does not fall back to plaintext
   and does not report a bare failure: it runs "discard the new grant" and reports "Couldn't
   finish connecting" together with that operation's disclosure sentence, which is the same
   sentence stage 2 shows.
5. **Connected.** The binding is written and, in the *same* blob mutation, any revocation job this
   installation still has pending for the same (project, `sub`) is discarded (decision 5).

**"Discard the new grant" — one operation, named once.** Stage 3, stage 4, the identity-mismatch
abort above and a declined account-change confirm (decision 2) all end holding a token the exchange
just issued and no right to keep it. They run the same ordered steps, stated here rather than
re-derived at four call sites that can drift apart:

1. **The token is dropped in memory.** Nothing is persisted: no binding record, no partial record,
   no revocation job, no cache row.
2. **Nothing is posted to the revocation endpoint** — not for a `sub` this installation already
   holds, and not for one it has never seen.
3. **The grant is disclosed in words**, inside the same failure message: Buzz may appear in that
   Google account's third-party access list until the user retries or removes it there, with a link
   to the Google account permissions page. That is stage 2's sentence, reused rather than
   paraphrased per stage.

**Why this operation does not revoke.** Revocation is project-granular (decision 5): posting the
token we have just received removes every scope that Google account granted this Cloud project and
invalidates every token issued under it, on every device that account uses. The only local thing
about that token is that we are holding it. Device A already runs a working Buzz connection for
account B; on device C, bound to account A, the user picks B and then declines the account-change
confirm — and a revoking discard would disconnect device A with a dialog the user just refused. The
identical harm reaches device A through the insufficient-scope stage, the persist-failure stage and
the identity-mismatch abort, none of which is a statement by the user about that account's other
devices. This is the harm decision 1 already refuses one section up for the same-`sub` case; the
only change is that the guard no longer asks whether *this installation* holds a record, which was
never the question that decided the blast radius.

What we accept instead is a grant with no local token. It is not silent, which is what `AGENTS.md`
Review-Proven Rule 1 asks of a caught failure: the failure message names the live grant, names
where to remove it, and the retry that replaces it is one button away. The earlier draft's
alternative — a durable revocation job for a grant the user never asked to end — satisfied the
letter of Rule 1 by scheduling the damage instead of reporting it.

The **confirmed** half of an account change is the one place a token issued by this flow still
leads to a revocation, and it is the previous binding's token that goes, not the new one
(decision 2). Disconnect and sign-out (decision 5) are the others. No other path in this contract
reaches the revocation endpoint.

T12 names a test per path into that operation: an exchange failure, an insufficient scope set, a
persist failure after a successful exchange, an identity switch between the exchange and the
persist, and a declined account-change confirm. Each asserts the same three things — **no request
reaches the revocation endpoint**, no journal entry is written, and no credential is persisted. The
decisive case is the two-installation test this rule exists for: installation 1 holds a live
binding for account B; on installation 2, bound to account A, the user completes consent for B and
declines the confirm; installation 1's binding must still work afterwards, and its next refresh must
not return `invalid_grant`.

None of these
states is "not shared with your account" (decisions 4 and 6): that message means a connected
account the calendar's ACL does not list, and showing it to someone who never reached the consent
screen sends them to an admin to fix an ACL that is not the problem.

**Reason.** The events scopes are the smallest pair that answers every question this surface asks:
one `events.list` on the mapped calendar returns the window, the calendar's `summary` and time
zone, and the caller's `accessRole`, without asking for calendar management and without depending
on a `CalendarList` entry Google no longer creates when a calendar is shared (decision 3).
`calendarlist.readonly` is asked for because a picker is the obvious next entry point and a second
consent to add it later is exactly what this decision refuses; it is kept out of the read set
because nothing in v1 breaks without it. Asking for the whole union at connect and deriving the
surface from the readback is what keeps "read-only member" a real state without a second
authorization: Google will not give an installed app an incremental upgrade, so a design that
depends on one would hand a teacher who drags a class a token carrying only `calendar.events`, and
the same readback rule that protects the read-only surface would then correctly record that
`events.readonly` is gone — trying to edit would break reading.
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
expiry, granted scopes, `sub`, email for display, and the `binding_generation` defined below — is
stored in the OS keychain through
`SecretStore` under a key namespaced by pubkey hex. `SecretStore` keeps all secrets as one JSON
blob (the `BLOB_KEY` username `secrets`, `desktop/src-tauri/src/secret_store.rs:42-44`; the
service name is not a constant in that file but comes from `keyring_service()`,
`desktop/src-tauri/src/app_state_keyring.rs:9-23`, which returns `buzz-desktop` for release builds
and a `buzz-desktop-dev*` service otherwise), so this costs no extra keychain prompt. Decision 5's
revocation journal is a *second, separate key* in that same blob; it is deliberately not a field
of this record, because it has to outlive it. Token exchange, refresh and every Google API call
happen in Rust. The webview receives a redacted status struct only: connected, email, granted
scopes, expiry, state. Connecting a different Google account takes an explicit confirm, and the
confirm can only be raised *after* the token exchange, because `sub` is the thing that tells us the
account differs. On confirm, the **previous** binding's grant is revoked exactly as decision 5's
Disconnect revokes it, and the confirm carries decision 5's sentence for it: this ends the previous
Google account's Buzz access on every device. On decline, the new token runs decision 1's "discard
the new grant" — no revocation of either account — and the previous binding is untouched. The
pubkey → Google-account mapping is never published to the relay.

**`binding_generation` — what it is, when it changes, and what it survives.** It is a field of the
binding record, not a process-local counter: 128 bits from the OS CSPRNG, drawn fresh whenever a
binding record is written, never derived from a clock, a sequence or a hash of the credential.

- **Minted** on first connect, and again on every *credential replacement* — a re-authorization
  that stores a new refresh token, and an account change. Every mint is a new CSPRNG draw and never
  an increment, so no caller can predict or reconstruct one.
- **Unchanged** by an ordinary access-token refresh, by an app restart, by a community switch and
  by every read. A refresh writes a new access token and expiry into the record and leaves the
  generation alone, which is what lets a cache row stay valid across an ordinary week.
- **Carried** by everything that outlives the process: decision 5's journal entries copy it,
  decision 6's cache rows key on it, and this decision's command handles are minted against it.
  Each compares against the value read back from the persisted record, never against one still held
  in memory from before a restart.
- **Gone** with the record. A binding deleted by Disconnect takes its generation with it and the
  next connect mints a new one, so nothing written before a Disconnect can match anything after it.

A process-local counter is the defect this field exists to prevent, and it fails in exactly the
place the fence matters: it resets to its initial value on relaunch, so a cache row written under a
replaced credential matches the record that replaced it, and decision 6's tuple stops fencing at
the moment a restart makes it load-bearing. T12 binds this to a **real store reopen** rather than
an in-memory double — replace the credential, close and reopen both the `SecretStore` and the cache
DB, then present a refresh response and a cache row from before the replacement. Both must be
rejected at the production persist and delivery seam, and the generation read back after the reopen
must equal the one written before it and differ from the one before the replacement.

**Where `sub` comes from.** The `sub` is read from the `id_token` in the token-endpoint response
Buzz receives directly from Google over TLS, and from nowhere else: never from a UserInfo call,
never from a value that passed through the webview, never from the loopback callback's query
string. Because that channel is direct and intermediary-free, Google's own OpenID Connect guidance
lets an app use the claims of a token received that way without full signature validation. That
guidance leans partly on the client secret authenticating the app to Google, which an installed app
does not have (decision 1 calls its secret an identifier, not a credential); here it is PKCE that
binds this response to this app, and the `aud` check below that binds the token to this client.
Buzz still checks that the `aud` claim equals this client id, and that check is not ceremony: it
is what stops a token minted for some other client from driving the account-change comparison and
being persisted as the binding identity. If a later ticket ever sources `sub` from anywhere but that
direct response, full ID-token validation — signature against the published JWKS, `iss`, `aud`,
`exp` — becomes required at that point, and the ticket that moves it owns that work.

**The command surface is part of this boundary.** Holding the token in Rust stops the *token* from
leaving the process; on its own it does not stop the token's *authority* from leaving, because the
renderer can still invoke the commands. So the calendar commands are constrained here:

- Every command takes an **opaque binding handle** minted in Rust — a random id valid only for the
  current active identity, the current community and the current binding generation. With the one
  exception named below, no command takes a caller-supplied calendar id and no command enumerates
  calendars.
- An event is addressed by an event handle drawn from the rows Rust itself delivered for the
  current window, never by a raw Google event id supplied by the caller.
- On every call Rust re-derives from its own state, not from arguments: the active identity pubkey,
  the current community, the channel-to-calendar mapping (decision 4), and the `accessRole` carried
  by the last `events.list` answer for that calendar (decision 3). Any mismatch rejects the call.

**The one exception, because decision 4 needs one.** The admin conveys the calendar id out of band
and each user sets the mapping locally, so *some* entry point has to accept an id the app has never
seen; the alternatives are a picker over the account's calendars, which the bullets above forbid,
or a decision 4 nobody can carry out. Leaving that unnamed is what would make an implementer either
ship the mapping unusable or quietly widen a list command, so it is named here and bounded:

- **`propose_calendar_mapping(channel_handle, raw_calendar_id)`** is the only command that accepts
  a raw calendar id. It returns **no calendar data of any kind** — not a summary, not a role, not
  an existence bit — and it mints no handle.
- Rust alone resolves the id: one `events.list` against it under decision 1's client limits, whose
  response carries the summary, the time zone and the `accessRole` that the confirmation and
  decision 3 need.
- Any of that is shown **only** in the OS-native confirmation outside the webview, which names the
  calendar summary and the Google account. The mapping is written, and a binding handle first
  minted for it, only after the user confirms there.
- The value returned to the renderer is one of exactly two: `confirmed` or `not_confirmed`. A
  calendar id that does not exist, one this account cannot read, a request that hit a client limit
  and a user who pressed Cancel are **indistinguishable** from the renderer's side, so a
  compromised renderer cannot turn the command into an existence oracle over the account's
  calendars.
- Proposals are rate-limited per identity, and the limit is a Rule 4 bound rather than a warning: a
  renderer that spends it receives `not_confirmed` and a logged line, never a faster answer.
- Nothing else moves. Raw calendar ids stay rejected by every list, read and edit command.

A Rust-owned picker over `calendarList` may be added later as a second entry point; it would end in
the same native confirmation and mint the same handles, and Rust, not the renderer, would
enumerate. It is not in v1 because decision 4's out-of-band convention does not need it, and
because `calendarList` is picker metadata and nothing else (decisions 3 and 6).

T12 names handler-level tests: a handle whose calendar is no longer in the current mapping is
rejected, and so is a request that carries a raw calendar id to any command but the proposal one; a
handle minted under a previous binding generation is rejected; a handle minted under another
identity is rejected; a handle minted in community A is rejected after a switch to B; a proposal
for a calendar id that does not exist and a proposal for one this account cannot read return the
identical `not_confirmed` value and nothing else; and a proposal whose native confirmation is
declined, or never resolves, writes no mapping and mints no handle.

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

Edit affordances derive from the `accessRole` carried by the **`events.list` response for the
mapped calendar** — the same call that fetches the window — and the table is closed:

| `accessRole` | Edit affordances |
|---|---|
| `owner`, `writer` | enabled for every event on the calendar |
| `writerWithoutPrivateAccess` | enabled for the events the API returns in full; disabled for events returned as free/busy only, which render as busy blocks with no edit affordance |
| `reader`, `freeBusyReader`, `none` | disabled |
| **any other value, present or future** | **disabled — treated as read-only** |

The last row is the rule, not a placeholder. Google adds roles; an unrecognized role must never
default to allow, because the resulting edit fails at Google with a raw error, which this decision
forbids, and it must not be an implementation choice, because "hide edit" and "default allow" are
both defensible in isolation and only one of them is safe.

**Why the role comes from `events.list` and not from `calendarList`.** Google documents that
sharing a calendar with a user no longer inserts it into that user's `CalendarList`. A correctly
shared teacher can therefore be absent from her own `calendarList` while her reads of the calendar
are fully authorized — and a role derived from `calendarList` would be *missing* for exactly that
user: no role, so no affordances, and, under an earlier draft of decision 6's matrix, a purge and
an ACL support call for an ACL that is correct. The `events.list` response carries `accessRole`
with all six values Google defines for it — `none`, `freeBusyReader`, `reader`,
`writerWithoutPrivateAccess`, `writer`, `owner` — beside the `summary` and `timeZone` the surface
needs, and it is authorized by the scopes decision 1 actually requests. One call answers "may I
read this calendar", "what may I do here" and "what is in the window", which is why decision 6 also
makes it the authoritative probe. `calendarList` keeps one job — offering calendars in a picker —
and decides nothing.

T12 tests one case per row, including an invented unknown role, and one case the rows alone would
not force: a `writer` teacher whose calendar is absent from her `calendarList`, who must still get
her events and her edit affordances.

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
| 403 `insufficientPermissions` on a **write** | write-authorization | edit affordances drop to read-only; if the scope readback shows `calendar.events` absent, decision 1's re-authorization is offered |
| 403 `insufficientPermissions` on a **read** | terminal auth (decision 8) | a scope the read surface depends on is no longer granted: `needs_reconnect`, events dropped as decision 8 states. Never access loss, and never the "no longer shared" message — the ACL is not what changed |
| 404 on an **event-level** request (get, patch or delete of one event id) | missing event | drop that one cached row and refresh the window; never access loss, never a message about sharing |
| 403 with an access reason, or 404, on the mapped calendar's **`events.list`**, still failing after one backed-off retry | access loss | purge that calendar's cached rows; surface "no longer shared with your account" |
| 404, or plain absence, from `calendarList.get` / `calendarList.list` | picker metadata only | the calendar is not offered in a picker. No purge, no message, no state change — see "Absence from `calendarList`" below |
| 401 on a resource call | expiry until proven otherwise | one generation-fenced forced refresh and one replay of that call; the refresh response classifies, never the 401 |
| an aborted, capped or page-bounded request (decision 1's client limits) | transient (decision 8) | keep the cache, back off; a stall proves nothing about the ACL or the grant |
| refresh returning `invalid_grant`, or a 401 on the replay after a refresh that succeeded | terminal auth (decision 8) | `needs_reconnect` |
| refresh returning `invalid_client` | app error (decision 8) | `app_error`; no reconnect affordance, because a reconnect cannot repair it |
| network failure, 5xx, 429 | transient (decision 8) | keep the cache, back off |

**The matrix closes on the operation, not only the status.** Google documents 404 for two
different things: a resource that never existed, and a calendar the user cannot access. An event
someone deleted in Google answers 404 to a read of its cached id, and answers 404 again to the
backed-off retry, because a deleted event stays deleted. Classified on status alone that satisfies
the access-loss condition, purges the whole calendar, and sends a teacher to an administrator to
hunt an ACL that is correct — the support call decision 6 already removed from the rate-limit row,
one row further down. So only the mapped calendar's **`events.list`** can establish access loss; an
event-level 404 removes that event and nothing else.

**One probe, and it is the one the scopes authorize.** `calendars.get` appears nowhere in this
contract. Google documents it as requiring one of `calendar.readonly`, `calendar`,
`calendar.app.created`, `calendar.calendars` or `calendar.calendars.readonly`, and decision 1
requests none of them — so every call would answer 403 `insufficientPermissions`, for every user,
forever, and land in whichever row caught it. An earlier draft named it as a calendar-level probe;
the row is deleted rather than repaired, because adding `calendar.calendars.readonly` would widen
the grant for a question `events.list` already answers. `calendarList.get` is out of the access-loss
condition for the reason decision 3 gives: absence from a `CalendarList` is an ordinary state for a
correctly shared calendar, so a 404 there is evidence about a picker and about nothing else. What
remains is one probe — `events.list` on the mapped calendar — which is authorized by
`calendar.events.readonly` and by `calendar.events`, both in decision 1's request, and which
returns the `accessRole` decision 3 reads and the window the view draws in the same response.

A read `insufficientPermissions` is therefore never an unclassified reason. It is the granted set
changing under a live binding — a scope withdrawn at the user's Google account page, or a
re-authorization that stored less than the surface depends on — which is a terminal auth condition
in decision 8's sense and not an ACL event. It drops the events the way decision 8 drops them and
it never says "no longer shared with your account", because sharing is not what moved.

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

T12 names a test per row, and five more that the rows alone would not force: a deleted event that
answers 404 twice (only that row disappears, no purge and no sharing message); an unknown 403
reason that repeats until the staleness ceiling (`unreachable`, never "no longer shared"); the
expiry-mid-call race — one 401, one refresh, one successful replay, no state change and no
Reconnect prompt — plus an assertion that five concurrent 401s produce exactly one refresh; a
calendar whose ACL is valid and which `calendarList` does not list, where `events.list` succeeds
and nothing purges; and a read answering 403 `insufficientPermissions`, which must reach
`needs_reconnect` without ever rendering the "no longer shared" message.

**Absence from `calendarList` is not evidence of anything.** The list call hides calendars for
three ordinary reasons: `showHidden` defaults to false, `maxResults` defaults to 100 entries with
`nextPageToken` paging, and — the one that matters most here — Google no longer inserts a shared
calendar into the recipient's `CalendarList` at all, so a calendar a teacher can fully read may
simply never have been listed for her. T12's list call therefore sets `showHidden=true` and pages
to exhaustion, or to decision 1's page and item bounds, rejecting a `nextPageToken` it has already
seen — and even then, absence only means "do not offer this calendar in the picker". It never
purges, never produces the "no longer shared" message, and never withholds the events: the mapping
of decision 4 and the `events.list` probe do not consult it. Only a classified failure on
`events.list` for the mapped calendar can establish access loss. T12 tests a calendar the user hid
in Google's own UI, a calendar sorted past entry 100, and a calendar that was shared correctly and
never inserted into the list at all: all three resolve, and none of them purges.

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
and keeps the binding when `sub` matches, without ever revoking. A different `sub` is an account
change and takes decision 2's explicit confirm, which is raised only after the exchange; declining
it revokes nothing either.

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

## Deviations

Named here so T12's PR body carries them forward instead of rediscovering them. A deviation is
something this memo does that the plan's T11 or T12 text does not say, or says differently.

Carried from earlier rounds, unchanged:

- **Filename.** The plan writes `docs/plans/2026-09-xx-calendar-authorization.md`; this file uses
  the resolved date, `2026-09-04-calendar-authorization.md`. The plan file itself was not edited to
  point at the resolved name, because other wave-1 tickets share that file and the edit would
  collide.
- **Two decisions beyond the literal checklist.** The flow shape — PKCE loopback, every token and
  every Google call in Rust, forced by the desktop CSP allowing `connect-src https:` — and the
  24-hour staleness ceiling. Checklist items 6 and 7 cannot be answered without them.
- **Two corrected citations.** The feature audit cites `VISION.md:35` and `ingest.rs:434`; in this
  tree they are `VISION.md:37` and `ingest.rs:437`, and this memo cites the latter.
- **v1 mapping authority.** The checklist asks who chooses which channels show the calendar.
  Decision 4 answers "the channel admin decides, out of band; each user sets it locally", because
  an enforced answer needs a relay-allow-listed kind this fork cannot add. The gap is stated in
  decision 4 and the ticket that closes it is in the Deferred list.

New in this round. Each widens what T12 must build or must test:

- **T12's mock Google server grows from one behavior to six.** The plan names "two principals, one
  calendar shared to both, and an ACL-loss case". This contract additionally requires the mock to
  serve: a connection that is accepted and then stalls, a response body past the byte cap, a cyclic
  `nextPageToken`, a calendar whose ACL is valid but which is absent from the user's `CalendarList`,
  and a read answering 403 `insufficientPermissions`.
- **`accessRole` comes from `events.list`, not from `calendarList`.** T12's named frontend test
  "disable edit when write ACL is absent" binds to the `accessRole` in the `events.list` response.
  `calendarList` gates nothing and can be absent for a correctly shared user.
- **One desktop command the plan does not name.** `propose_calendar_mapping` (decision 2) is the
  only way a calendar id reaches Rust under decision 4's out-of-band convention. It is outside what
  "per-user OAuth through desktop commands" describes in T12, and it carries its own tests —
  including the one asserting that a non-existent calendar and an unreadable one are
  indistinguishable to the caller.
- **The keychain record gains a persisted field.** `binding_generation` (decision 2) lives inside
  the `SecretStore` blob and must survive a store reopen. That is more than "tokens in the keychain
  (`secret_store` pattern)" implies in T12, and it is the value T12's cache and journal fences are
  checked against, so its test uses a real store reopen rather than an in-memory double.
- **No revocation on a failed or declined connect.** T12's traceability table cannot list a
  revocation test for a declined confirm, an insufficient scope set, a persist failure or an
  identity switch; the assertion for all four is the opposite one — no request reaches the
  revocation endpoint. Revocation appears only under Disconnect, the confirmed half of an account
  change, and sign-out. The plan's "revocation propagation within the bounded window" test is
  unaffected: it exercises decision 7, which is a poll bound, not a discard path.
- **A shared HTTP client with stated limits.** Decision 1's deadlines, byte cap, page and item
  bounds, repeated-page-token rejection and cancellation points are a T12 component the plan does
  not name, and three of its tests need a server that misbehaves rather than one that answers.

## Relates to

- Upstream RFC #3227 — app-integration agents with scoped credentials (the shape decision 9
  follows; it does not cover decisions 1–8).
- Upstream PR #1382 — the closed Google Calendar work T12 revives for the OAuth and storage half.
- `2026-09-04-zs-feature-audit.md` §4 — the audit that ruled out a native kind, Cal.com and
  iframes.
