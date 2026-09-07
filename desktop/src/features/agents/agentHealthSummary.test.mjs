import assert from "node:assert/strict";
import test from "node:test";

import { buildHealthRows, hasHealthAlertBadge } from "./agentHealthSummary.ts";

test("reducesCountersToRow", () => {
  const now = "2026-09-06T12:00:00Z";

  const counters = {
    summary24h: [
      {
        agent: "agent_alpha",
        turns: 12,
        failed: 1,
        parked: 2,
        needsReview: 0,
        reconnects: 4,
        lastFailureClass: "rate_limit",
        lastFailureAt: 1725600000,
        latestPausedUntil: null,
        breakerOpen: false,
      },
    ],
    summary7d: [
      {
        agent: "agent_alpha",
        turns: 48,
        failed: 3,
        parked: 2,
        needsReview: 0,
        reconnects: 10,
        lastFailureClass: "rate_limit",
        lastFailureAt: 1725600000,
        latestPausedUntil: null,
        breakerOpen: false,
      },
    ],
  };

  const managedAgents = [
    { pubkey: "agent_alpha", name: "Alpha Agent" },
    { pubkey: "agent_beta", name: "Beta Agent" },
  ];

  const runtimes = [{ pubkey: "agent_alpha", lifecycle: "ready" }];

  const rows = buildHealthRows(counters, managedAgents, runtimes, now);

  assert.equal(rows.length, 2);

  const alphaRow = rows.find((r) => r.pubkey === "agent_alpha");
  assert.ok(alphaRow, "agent_alpha row must exist");
  assert.equal(alphaRow.pubkey, "agent_alpha");
  assert.equal(alphaRow.name, "Alpha Agent");
  assert.equal(alphaRow.state, "active");
  assert.equal(alphaRow.pausedUntil, null);
  assert.equal(alphaRow.turns24h, 12);
  assert.equal(alphaRow.turns7d, 48);
  assert.equal(alphaRow.failed24h, 1);
  assert.equal(alphaRow.failed7d, 3);
  assert.equal(alphaRow.parked, 2);
  assert.equal(alphaRow.needsReview, 0);
  assert.equal(alphaRow.lastErrorClass, "rate_limit");
  assert.equal(alphaRow.lastErrorAt, 1725600000);
  assert.equal(alphaRow.reconnects24h, 4);

  const betaRow = rows.find((r) => r.pubkey === "agent_beta");
  assert.ok(betaRow, "agent_beta row must exist");
  assert.equal(betaRow.pubkey, "agent_beta");
  assert.equal(betaRow.name, "Beta Agent");
  assert.equal(betaRow.state, "offline");
  assert.equal(betaRow.pausedUntil, null);
  assert.equal(betaRow.turns24h, 0);
  assert.equal(betaRow.turns7d, 0);
  assert.equal(betaRow.failed24h, 0);
  assert.equal(betaRow.failed7d, 0);
  assert.equal(betaRow.parked, 0);
  assert.equal(betaRow.needsReview, 0);
  assert.equal(betaRow.lastErrorClass, null);
  assert.equal(betaRow.lastErrorAt, null);
  assert.equal(betaRow.reconnects24h, 0);
});

test("pausedUntilWinsOverActive", () => {
  const now = "2026-09-06T12:00:00Z";

  const countersFuturePause = {
    summary24h: [
      {
        agent: "agent_alpha",
        turns: 5,
        failed: 0,
        parked: 0,
        needsReview: 0,
        reconnects: 0,
        latestPausedUntil: "2026-09-06T14:00:00Z",
        breakerOpen: false,
      },
    ],
    summary7d: [],
  };

  const managedAgents = [{ pubkey: "agent_alpha", name: "Alpha Agent" }];
  const runtimes = [{ pubkey: "agent_alpha", lifecycle: "ready" }];

  // Future pause: pause wins over active runtime
  const rowsFuture = buildHealthRows(
    countersFuturePause,
    managedAgents,
    runtimes,
    now,
  );
  assert.equal(rowsFuture[0].state, "paused");
  assert.equal(rowsFuture[0].pausedUntil, "2026-09-06T14:00:00Z");

  // Past/expired pause: runtime state (active) applies
  const countersExpiredPause = {
    summary24h: [
      {
        agent: "agent_alpha",
        turns: 5,
        failed: 0,
        parked: 0,
        needsReview: 0,
        reconnects: 0,
        latestPausedUntil: "2026-09-06T10:00:00Z",
        breakerOpen: false,
      },
    ],
    summary7d: [],
  };

  const rowsExpired = buildHealthRows(
    countersExpiredPause,
    managedAgents,
    runtimes,
    now,
  );
  assert.equal(rowsExpired[0].state, "active");
});

test("badgeWhenNeedsReviewOrBreakerOpen", () => {
  // Needs review > 0 triggers badge
  assert.equal(
    hasHealthAlertBadge([
      {
        pubkey: "agent_1",
        name: "Agent 1",
        state: "active",
        pausedUntil: null,
        turns24h: 1,
        turns7d: 1,
        failed24h: 0,
        failed7d: 0,
        parked: 1,
        needsReview: 1,
        lastErrorClass: null,
        lastErrorAt: null,
        reconnects24h: 0,
      },
    ]),
    true,
  );

  // Breaker open (state === "breaker") triggers badge
  assert.equal(
    hasHealthAlertBadge([
      {
        pubkey: "agent_2",
        name: "Agent 2",
        state: "breaker",
        pausedUntil: null,
        turns24h: 0,
        turns7d: 0,
        failed24h: 1,
        failed7d: 1,
        parked: 0,
        needsReview: 0,
        lastErrorClass: "rate_limit",
        lastErrorAt: 1725600000,
        reconnects24h: 0,
      },
    ]),
    true,
  );

  // Both needsReview > 0 and breaker open
  assert.equal(
    hasHealthAlertBadge([
      {
        pubkey: "agent_3",
        name: "Agent 3",
        state: "breaker",
        pausedUntil: null,
        turns24h: 1,
        turns7d: 1,
        failed24h: 1,
        failed7d: 1,
        parked: 1,
        needsReview: 2,
        lastErrorClass: "rate_limit",
        lastErrorAt: 1725600000,
        reconnects24h: 0,
      },
    ]),
    true,
  );
});

test("noBadgeWhenClean", () => {
  // Empty or nullish
  assert.equal(hasHealthAlertBadge([]), false);
  assert.equal(hasHealthAlertBadge(null), false);
  assert.equal(hasHealthAlertBadge(undefined), false);

  // Clean active rows
  assert.equal(
    hasHealthAlertBadge([
      {
        pubkey: "agent_1",
        name: "Agent 1",
        state: "active",
        pausedUntil: null,
        turns24h: 10,
        turns7d: 20,
        failed24h: 0,
        failed7d: 0,
        parked: 0,
        needsReview: 0,
        lastErrorClass: null,
        lastErrorAt: null,
        reconnects24h: 0,
      },
    ]),
    false,
  );

  // Clean paused rows
  assert.equal(
    hasHealthAlertBadge([
      {
        pubkey: "agent_2",
        name: "Agent 2",
        state: "paused",
        pausedUntil: "2026-09-06T14:00:00Z",
        turns24h: 5,
        turns7d: 10,
        failed24h: 0,
        failed7d: 0,
        parked: 0,
        needsReview: 0,
        lastErrorClass: null,
        lastErrorAt: null,
        reconnects24h: 0,
      },
    ]),
    false,
  );

  // Clean offline rows
  assert.equal(
    hasHealthAlertBadge([
      {
        pubkey: "agent_3",
        name: "Agent 3",
        state: "offline",
        pausedUntil: null,
        turns24h: 0,
        turns7d: 0,
        failed24h: 0,
        failed7d: 0,
        parked: 0,
        needsReview: 0,
        lastErrorClass: null,
        lastErrorAt: null,
        reconnects24h: 0,
      },
    ]),
    false,
  );
});
