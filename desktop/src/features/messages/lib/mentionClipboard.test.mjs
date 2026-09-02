import assert from "node:assert/strict";
import test from "node:test";

import {
  buildMentionClipboardHtml,
  getBuzzCopyKind,
  parseMentionClipboardRecords,
  registerMentionClipboardIdentities,
  selectVisibleMentionIdentities,
} from "./mentionClipboard.ts";

const JOHN = "a".repeat(64);
const ALEX = "b".repeat(64);
const ALEX_KIM = "c".repeat(64);
const FIZZ = "d".repeat(64);

const john = { label: "John Smith", pubkey: JOHN, isAgent: false };

// ── buildMentionClipboardHtml ─────────────────────────────────────────

test("wraps a multi-word mention with its identity", () => {
  const html = buildMentionClipboardHtml({
    text: "@John Smith fixed the bug",
    identities: [john],
  });

  assert.equal(
    html,
    '<span data-buzz-copy="markdown">' +
      `<span data-mention="" data-mention-pubkey="${JOHN}" ` +
      'data-mention-kind="human" data-mention-label="John Smith">' +
      "@John Smith</span> fixed the bug</span>",
  );
});

test("returns null when the body has no known mention", () => {
  assert.equal(
    buildMentionClipboardHtml({
      text: "just a plain message",
      identities: [john],
    }),
    null,
  );
  assert.equal(
    buildMentionClipboardHtml({ text: "@John Smith", identities: [] }),
    null,
  );
});

test("marks agent mentions so the re-lit chip keeps its kind", () => {
  const html = buildMentionClipboardHtml({
    text: "ping @Fizz",
    identities: [{ label: "Fizz", pubkey: FIZZ, isAgent: true }],
  });

  assert.match(html, /data-mention-kind="agent"/);
});

test("longest display name wins when one prefixes another", () => {
  const html = buildMentionClipboardHtml({
    text: "@Alex Kim shipped it",
    identities: [
      { label: "Alex", pubkey: ALEX, isAgent: false },
      { label: "Alex Kim", pubkey: ALEX_KIM, isAgent: false },
    ],
  });

  assert.match(html, new RegExp(`data-mention-pubkey="${ALEX_KIM}"`));
  assert.doesNotMatch(html, new RegExp(`data-mention-pubkey="${ALEX}"`));
  assert.match(html, />@Alex Kim<\/span>/);
});

test("keeps the casing the author wrote", () => {
  const html = buildMentionClipboardHtml({
    text: "@john smith fixed it",
    identities: [john],
  });

  assert.match(html, />@john smith<\/span>/);
  assert.match(html, /data-mention-label="John Smith"/);
});

test("does not wrap mentions inside code spans or fences", () => {
  assert.equal(
    buildMentionClipboardHtml({
      text: "`@John Smith`",
      identities: [john],
    }),
    null,
  );
  assert.equal(
    buildMentionClipboardHtml({
      text: "```\n@John Smith\n```",
      identities: [john],
    }),
    null,
  );
});

test("wraps every occurrence and escapes the surrounding text", () => {
  const html = buildMentionClipboardHtml({
    text: "@John Smith & <b> @John Smith",
    identities: [john],
  });

  assert.equal(html.match(/data-mention=""/g).length, 2);
  assert.match(html, / &amp; &lt;b&gt; /);
});

test("newlines become line breaks in the html flavor", () => {
  const html = buildMentionClipboardHtml({
    text: "@John Smith\nsecond line",
    identities: [john],
  });

  assert.match(html, /<br>second line/);
});

test("ignores identities without a well-formed pubkey", () => {
  assert.equal(
    buildMentionClipboardHtml({
      text: "@John Smith",
      identities: [{ label: "John Smith", pubkey: "nope", isAgent: false }],
    }),
    null,
  );
});

test("rich copies declare their own flavor", () => {
  const html = buildMentionClipboardHtml({
    text: "@John Smith",
    identities: [john],
    kind: "rich",
  });

  assert.match(html, /data-buzz-copy="rich"/);
});

// ── getBuzzCopyKind ───────────────────────────────────────────────────

test("reads the copy marker, and only Buzz's", () => {
  assert.equal(
    getBuzzCopyKind('<span data-buzz-copy="markdown">hi</span>'),
    "markdown",
  );
  assert.equal(getBuzzCopyKind('<div data-buzz-copy="rich">hi</div>'), "rich");
  assert.equal(getBuzzCopyKind("<p>hi</p>"), null);
  assert.equal(getBuzzCopyKind('<span data-buzz-copy="other">hi</span>'), null);
});

// ── parseMentionClipboardRecords ──────────────────────────────────────

test("recovers records from a Buzz copy", () => {
  const records = parseMentionClipboardRecords(
    buildMentionClipboardHtml({
      text: "@John Smith and @Fizz",
      identities: [john, { label: "Fizz", pubkey: FIZZ, isAgent: true }],
    }),
  );

  assert.deepEqual(records, [
    { label: "John Smith", pubkey: JOHN, isAgent: false },
    { label: "Fizz", pubkey: FIZZ, isAgent: true },
  ]);
});

test("recovers records from single-quoted, reordered attributes", () => {
  const records = parseMentionClipboardRecords(
    `<span data-mention-label='Jo &amp; Ann' data-mention-kind='human' ` +
      `data-mention-pubkey='${JOHN}' data-mention="">@Jo &amp; Ann</span>`,
  );

  assert.deepEqual(records, [
    { label: "Jo & Ann", pubkey: JOHN, isAgent: false },
  ]);
});

test("rejects malformed pubkeys", () => {
  for (const pubkey of ["", "zz", `${JOHN}0`, "g".repeat(64)]) {
    assert.deepEqual(
      parseMentionClipboardRecords(
        `<span data-mention-pubkey="${pubkey}" data-mention-label="John">@John</span>`,
      ),
      [],
      `expected ${pubkey} to be rejected`,
    );
  }
});

test("normalizes pubkey casing on both sides of the round trip", () => {
  const upper = "A".repeat(64);
  assert.deepEqual(
    parseMentionClipboardRecords(
      `<span data-mention-pubkey="${upper}" data-mention-label="John">@John</span>`,
    ),
    [{ label: "John", pubkey: JOHN, isAgent: false }],
  );
  assert.match(
    buildMentionClipboardHtml({
      text: "@John",
      identities: [{ label: "John", pubkey: upper, isAgent: false }],
    }),
    new RegExp(`data-mention-pubkey="${JOHN}"`),
  );
});

test("rejects records without a label and oversized labels", () => {
  assert.deepEqual(
    parseMentionClipboardRecords(
      `<span data-mention-pubkey="${JOHN}">@John</span>`,
    ),
    [],
  );
  assert.deepEqual(
    parseMentionClipboardRecords(
      `<span data-mention-pubkey="${JOHN}" data-mention-label="${"x".repeat(201)}">@x</span>`,
    ),
    [],
  );
});

test("caps how many records one paste can register", () => {
  const spans = Array.from(
    { length: 80 },
    (_unused, index) =>
      `<span data-mention-pubkey="${index.toString(16).padStart(64, "0")}" ` +
      `data-mention-label="User ${index}">@User ${index}</span>`,
  ).join("");

  assert.equal(parseMentionClipboardRecords(spans).length, 50);
});

test("does not report the same identity twice", () => {
  const records = parseMentionClipboardRecords(
    buildMentionClipboardHtml({
      text: "@John Smith and @John Smith",
      identities: [john],
    }),
  );

  assert.equal(records.length, 1);
});

test("finds nothing in foreign clipboard html", () => {
  assert.deepEqual(
    parseMentionClipboardRecords("<p>data-mention-pubkey is a string</p>"),
    [],
  );
});

// ── selectVisibleMentionIdentities ────────────────────────────────────

const fizz = { label: "Fizz", pubkey: FIZZ, isAgent: true };

test("keeps identities the inserted content mentions", () => {
  assert.deepEqual(
    selectVisibleMentionIdentities([john, fizz], "@John Smith fixed the bug"),
    [john],
  );
});

test("drops an identity the inserted content never mentions", () => {
  // The hostile shape: a hidden record on copied HTML would otherwise rebind
  // a real member's display name for the rest of the composer session.
  assert.deepEqual(selectVisibleMentionIdentities([john], "look at this"), []);
  // The bare name is not a mention — only the sigil form binds.
  assert.deepEqual(
    selectVisibleMentionIdentities([john], "John Smith fixed the bug"),
    [],
  );
});

test("holds a partial label to the same standard as the extractor", () => {
  assert.deepEqual(
    selectVisibleMentionIdentities([john], "@John fixed it"),
    [],
  );
});

test("ignores a mention the extractor would mask as code", () => {
  assert.deepEqual(selectVisibleMentionIdentities([john], "`@John Smith`"), []);
  assert.deepEqual(
    selectVisibleMentionIdentities([john], "```\n@John Smith\n```"),
    [],
  );
});

// ── registerMentionClipboardIdentities ────────────────────────────────

test("registers each recovered pair with its agent flag", () => {
  const registered = [];
  registerMentionClipboardIdentities({
    html: buildMentionClipboardHtml({
      text: "@John Smith and @Fizz",
      identities: [john, fizz],
    }),
    registerMentionPubkey: (displayName, pubkey, options) =>
      registered.push([displayName, pubkey, options?.isAgent]),
    text: "@John Smith and @Fizz",
  });

  assert.deepEqual(registered, [
    ["John Smith", JOHN, false],
    ["Fizz", FIZZ, true],
  ]);
});

test("registers nothing for a record the paste does not show", () => {
  const registered = [];
  // A crafted sidecar: an empty span rebinding a name the content never
  // carries, riding alongside one the user can actually see.
  registerMentionClipboardIdentities({
    html:
      `<span data-mention="" data-mention-pubkey="${ALEX}" ` +
      `data-mention-label="John Smith"></span>` +
      `<span data-mention="" data-mention-pubkey="${FIZZ}" ` +
      `data-mention-kind="agent" data-mention-label="Fizz">@Fizz</span>`,
    registerMentionPubkey: (displayName, pubkey) =>
      registered.push([displayName, pubkey]),
    text: "@Fizz take a look",
  });

  assert.deepEqual(registered, [["Fizz", FIZZ]]);
});
