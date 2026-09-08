# Calendar completion

The September 8 user request is to make the calendar work, using Devin's Google
account, with placement delegated to the implementer. The merged T12 work is a
backend foundation, not a usable calendar: there is no connection command,
signature verifier, cache, or calendar screen.

## Product behavior

- Calendar is a primary sidebar destination next to Inbox and Projects.
- The default calendar belongs to the connected Google account. It is personal
  to the current Buzz identity, not published to a channel or an agent.
- Settings contains connection status and Connect/Disconnect. Credentials stay
  in the native secret store; the renderer receives status and bounded events.
- Month and agenda views use the existing calendar and virtualized-list
  primitives. All-day dates remain date strings with exclusive end dates;
  timed events use the viewer's local zone. A selected date shows its events.
- The fetch horizon remains 30 days back and 90 days ahead. Partial, stale,
  disconnected, and failed states are explicit. Unknown days are never presented
  as an authoritative empty calendar.
- Create/edit/delete use the existing Google transport, bounded fields,
  client-generated event IDs and ETags. Mutations require a current successful
  authority refresh, use only the native selected calendar, and fail closed on
  identity or binding changes. Tests never invite people or send notifications.

This deliberately changes T11's initial presentation from channel opt-in to a
personal sidebar surface. Connecting a personal account does not grant its
calendar to channel members. Shared-calendar mapping remains a separate explicit
Google authorization, not an inference from Buzz membership.

## Native boundaries

1. Use a Buzz-specific Google Desktop OAuth client and the existing PKCE,
   loopback, state, nonce and claim validation code. Verify RS256 signatures
   against bounded Google JWKS. Pin HTTPS endpoints, disallow redirects/proxy
   inheritance, bound response bytes and time. Never log callback URLs/tokens.
2. Serialize connection transitions per identity. Re-read active identity at
   commit and use the existing generation-fenced envelope mutations.
3. Keep event data in a separate bounded cache; enforce row, byte, partition and
   absolute staleness limits. Mutation authority never comes from cached UI.
4. Disconnect clears active authority and preserves retryable revocation custody
   in the existing journal. Purge and revocation outcomes are independently
   recorded and visible. Do not erase failed revocations or silently reconnect.
5. Calendar commands are human UI commands only. No agent/CLI/MCP calendar adapter
   or token environment is added.

## Verification

- Run existing calendar provider mocks, including lost create response, ETag
  conflict, callback state/nonce, membership-era foundation tests, caps and races.
- Add production-wiring tests for OAuth signature validation, identity changes,
  read/mutation fences, cache expiry and disconnect retry custody.
- Browser tests exercise month/agenda/date selection, partial and stale states,
  create/edit/delete forms, failures and keyboard navigation.
- Connect the confirmed Google account in the installed app, read actual events,
  and use a clearly labelled temporary event with no attendees for mutation
  verification. Remove it afterward and verify provider state.
- A configured UI or mock pass is not live Google verification. Any account,
  OAuth consent, or admin blocker remains explicit until resolved.

References: existing calendar authorization/view designs and Google's Desktop
OAuth and OpenID Connect documentation. No Google Cloud billing activation is
needed or authorized by this plan.

## Installation configuration

The native provider reads `BUZZ_GOOGLE_CALENDAR_CLIENT_ID` and optional
`BUZZ_GOOGLE_CALENDAR_CLIENT_SECRET` from the runtime environment, falling back to
values supplied at compile time. Finder-launched bundles therefore need the
Buzz-specific Desktop OAuth client metadata at build time. These are OAuth client
configuration values; account refresh/access tokens remain in native encrypted
storage and must never be added to a build or renderer configuration.

Without client configuration, Settings explicitly reports installation setup is
required. Disconnect and pending-cleanup recovery remain available. Background
cleanup status refreshes in the UI; stopping unsuccessful revocation retries is an
explicit user action and does not claim that Google revoked access.
