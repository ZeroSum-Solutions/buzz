# Calendar authorization contract (T11)

Date: 2026-09-04. Owner ticket: T11 `docs/calendar-authz`; implemented by T12 `feat/google-calendar`
and assumed by T12a. Buzz renders a Google Workspace calendar the viewer already has access to: it
owns no calendar and grants no access, so effective access is the intersection of Buzz channel
membership and Google's calendar ACL. Upstream RFC #3227 supplies decision 9 and nothing else.

## Decisions

**1. OAuth scopes requested.** Decision: installed-app PKCE code flow, loopback redirect, Buzz-only
Cloud project (Internal, Production); scopes `openid`, `userinfo.email`, `calendar.events.readonly`,
`calendar.events` and nothing wider — no `calendarList` scope or call. The loopback listener bounds
bytes, sockets and read time. Reason: minimum-scope policy; Rule 4 bounds the listener.

**2. Google account to Buzz identity.** Decision: one Google account per identity per installation,
identity pubkey hex to the OIDC `sub` of an ID token validated for signature (cached JWKS), `iss`,
`aud` and `exp`. `SecretStore` keeps tokens, scopes, `sub`, email, issuing `client_id` and a CSPRNG
`binding_generation`; no token reaches the webview. Reason: PKCE protects the code, not the JWT.

**3. Calendar ownership and sharing.** Decision: a Workspace-owned secondary calendar, shared only
in Google's own ACL, which Buzz never reads, writes or reconciles. Affordances come from the
`accessRole` in the mapped calendar's `events.list`, narrowed by event type and organizer; an
unrecognized role is read-only. Reason: a calendar role does not make every event mutable.

**4. Which channels show it, and who chooses.** Decision: opt-in per channel, keyed by (pubkey,
community relay id, channel id), stored locally per identity; the admin conveys the calendar out of
band. The calendar id is typed into the native confirm, naming account, calendar and channel.
Reason: the mapping carries no authority; the renderer must not pick the destination.

**5. Disconnect behavior.** Decision: one `SecretStore` blob mutation inserts a revocation journal
entry (refresh token, binding key, generation, purge predicate, deadline) and removes the binding
together; then the purge, then Google's revocation endpoint. Only HTTP 200 clears the entry. Reason:
Rule 5 leaves no torn prefix; `invalid_token` does not prove the grant is gone.

**6. Cached event data on disconnect and membership loss.** Decision: a bounded SQLite render cache
outside the archive DB, keyed by (pubkey, community, binding generation, `sub`, calendar id, event
id), purged on both. Only a verified `events.list` failure purges for access loss; anything else
stays stale to a 24-hour ceiling. Reason: T12 requires the purge.

**7. Revocation propagation timing.** Decision: poll-bound — refresh on channel focus and at most
five minutes apart while the surface is visible, with backoff, so a principal removed in Google
keeps the view for one poll interval at worst, or until next open; 24 hours offline is the outer
bound. Reason: a webhook needs a public callback the fork does not want.

**8. Refresh-token failure UX.** Decision: transient (network, 5xx, 429, quota 403s) backs off
behind a stale view; `unreachable` past the 24-hour ceiling drops events, offering Retry; terminal
(`invalid_grant`, a withdrawn scope, a 401 surviving one forced refresh) offers Reconnect;
`invalid_client` is `app_error` with neither. Reason: Rules 6 and 4; honest state names.

**9. What an agent may read or write.** Decision: nothing in v1 — no managed agent, ACP harness or
MCP server gets a Google credential and no calendar command is agent-facing, bound by a test whose
removal fails. Later access takes RFC #3227's shape: a distinct Google principal, substituted at
egress. Reason: a human's grant lets an agent act as that human outside Buzz's gate.

## Risks accepted

- Revocation is Cloud-project-wide and cross-device: Disconnect on one machine ends the grant on
  every machine, and the other learns of it as `needs_reconnect` at its next refresh.
- A sign-out wipe attempts every pending revocation under one deadline and then proceeds; a grant it
  could not revoke stays live, and the confirm names that account.
- The v1 mapping is a local per-user setting, so the admin's choice is a convention no client
  enforces. Enforcing it needs a relay-allow-listed kind this fork cannot add.
- The Internal OAuth client admits only members of the Workspace that owns it; an outside guest can
  hold real read access in Google and still get no Buzz surface.
- Propagation is poll-bound, not immediate, and offline staleness is bounded only by the 24-hour
  ceiling.
- Membership loss purges the local cache while the user's Google access is unchanged, so the
  calendar returns intact at the next connect or mapping.
- Buzz gains a second authorization system beside the relay, which `VISION.md:37` does not
  anticipate. The fork accepts that tension rather than claiming the model is intact.
- T12's "refresh failure surfaces a reconnect state" is narrowed here to the terminal class; T12's
  traceability table follows decision 8's four states.
- The filename resolves the plan's `2026-09-xx-calendar-authorization.md`; the plan text is left
  alone, because other wave-1 tickets share that file.
