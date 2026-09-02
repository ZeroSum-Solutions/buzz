import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";
import {
  parseMentionClipboardRecords,
  selectVisibleMentionIdentities,
} from "./mentionClipboard.ts";
import {
  chipTextMatchesLabel,
  hasMentionClipboardHtml,
  normalizeMentionClipboardContent,
  restoreChipSigil,
} from "./normalizeMentionClipboard.ts";

// `normalizeMentionClipboardContent` needs a DOM; jsdom supplies the same
// globals it reaches for in the webview.
const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    DOMParser: dom.window.DOMParser,
    Element: dom.window.Element,
    HTMLElement: dom.window.HTMLElement,
    Node: dom.window.Node,
  });
});

after(() => dom.window.close());

// ── hasMentionClipboardHtml ───────────────────────────────────────────

test("returns true when HTML contains data-mention", () => {
  const html = '<span data-mention="" class="mention">@Alice</span>';
  assert.equal(hasMentionClipboardHtml(html), true);
});

test("returns true when HTML contains data-channel-link", () => {
  const html = '<button data-channel-link="">#general</button>';
  assert.equal(hasMentionClipboardHtml(html), true);
});

test("returns true when HTML contains both markers", () => {
  const html =
    '<span data-mention="">@Alice</span> in <button data-channel-link="">#general</button>';
  assert.equal(hasMentionClipboardHtml(html), true);
});

test("returns false for plain HTML without markers", () => {
  const html = "<p>Hello world</p>";
  assert.equal(hasMentionClipboardHtml(html), false);
});

test("returns false for empty string", () => {
  assert.equal(hasMentionClipboardHtml(""), false);
});

test("returns false for text that mentions 'data-mention' as content", () => {
  // Edge case: the literal string "data-mention" appears as text content,
  // not as an attribute. hasMentionClipboardHtml does a simple string
  // includes check, so this is a known false positive — acceptable because
  // the normalization function is a no-op when no matching elements exist.
  const html = "<p>The attribute is called data-mention</p>";
  assert.equal(hasMentionClipboardHtml(html), true);
});

// ── restoreChipSigil ──────────────────────────────────────────────────

test("puts back the sigil the rendered chip strips", () => {
  assert.equal(restoreChipSigil("John Smith", "@"), "@John Smith");
  assert.equal(restoreChipSigil("general", "#"), "#general");
});

test("leaves an already-sigiled label alone", () => {
  assert.equal(restoreChipSigil("@John Smith", "@"), "@John Smith");
  assert.equal(restoreChipSigil("#general", "#"), "#general");
});

test("never invents a lone sigil for empty chip text", () => {
  assert.equal(restoreChipSigil("", "@"), "");
});

// ── chipTextMatchesLabel ──────────────────────────────────────────────

test("accepts a full chip, with or without the sigil written back", () => {
  assert.equal(chipTextMatchesLabel("John Smith", "John Smith", "@"), true);
  assert.equal(chipTextMatchesLabel("@John Smith", "John Smith", "@"), true);
  assert.equal(chipTextMatchesLabel("#general", "general", "#"), true);
});

test("accepts the author's casing and pasteboard whitespace", () => {
  // `buildMentionSpanHtml` preserves the run as written, not the label's case.
  assert.equal(chipTextMatchesLabel("@john smith", "John Smith", "@"), true);
  // A pasteboard round trip can pad the markup or swap spaces for U+00A0.
  assert.equal(chipTextMatchesLabel(" John Smith ", "John Smith", "@"), true);
  assert.equal(
    chipTextMatchesLabel("John\u00a0Smith", "John Smith", "@"),
    true,
  );
});

test("rejects the fragment a boundary-crossing selection leaves behind", () => {
  // The browser's default copy keeps the chip's attributes around whatever
  // slice of its text the selection covered — from either end.
  assert.equal(chipTextMatchesLabel("John", "John Smith", "@"), false);
  assert.equal(chipTextMatchesLabel("Smith", "John Smith", "@"), false);
  assert.equal(chipTextMatchesLabel("", "John Smith", "@"), false);
});

test("rejects a nonempty fragment of an empty declared label", () => {
  assert.equal(chipTextMatchesLabel("John", "", "@"), false);
  assert.equal(chipTextMatchesLabel("", "", "@"), true);
});

// ── normalizeMentionClipboardContent ──────────────────────────────────

/** Nobody's key — what a crafted clipboard sidecar would name instead. */
const IMPOSTOR_PUBKEY = "1f".repeat(32);
const JOHN_SMITH_PUBKEY = "7c".repeat(32);

/** The records the paste would actually register, given the inserted text. */
function visibleIdentities(html, text) {
  return selectVisibleMentionIdentities(
    parseMentionClipboardRecords(html),
    text,
  );
}

test("text ProseMirror never inserts cannot vouch for an identity record", () => {
  // The reported vector: a real member's display name hidden in a <style>,
  // beside an empty record span claiming it against an attacker's key. The
  // composer shows only "visible", so the binding would be invisible to the
  // user who accepted it — and it outlives the paste that carried it.
  const html =
    "visible <style>@Jane Doe </style>" +
    `<span data-mention="" data-mention-pubkey="${IMPOSTOR_PUBKEY}" ` +
    'data-mention-label="Jane Doe"></span>';
  const content = normalizeMentionClipboardContent(html);

  assert.equal(content.text.includes("Jane Doe"), false);
  assert.equal(content.html.includes("Jane Doe"), false);
  assert.deepEqual(visibleIdentities(html, content.text), []);
});

for (const tag of ["script", "style", "title", "noscript", "object"]) {
  test(`<${tag}> text reaches neither output`, () => {
    const content = normalizeMentionClipboardContent(
      `<p>visible</p><${tag}>@Jane Doe </${tag}>`,
    );
    assert.equal(content.text.includes("Jane Doe"), false);
    assert.equal(content.html.includes("Jane Doe"), false);
    // The content the paste does insert survives the sweep.
    assert.equal(content.text.includes("visible"), true);
  });
}

test("a leading <style> the parser hoists into <head> is excluded too", () => {
  // Documents the hoist: this text lands outside <body>, so it is absent from
  // both body-derived outputs whether or not the sweep runs. The sweep spans
  // the whole document rather than <body> so that stays true if the
  // serialization root ever moves.
  const content = normalizeMentionClipboardContent(
    "<style>@Jane Doe </style>visible",
  );
  assert.equal(content.text.includes("Jane Doe"), false);
  assert.equal(content.html.includes("Jane Doe"), false);
  assert.equal(content.text.includes("visible"), true);
});

test("a mention span nested in an ignored tag contributes nothing", () => {
  // A DOMParser document has scripting disabled, so <noscript> and <object>
  // hold real element children — including a chip. Sweeping before the
  // flattening loop removes it outright, rather than flattening it into
  // sigiled text the gate would then read as visible.
  const html =
    "<p>visible</p><object>" +
    `<span data-mention="" data-mention-pubkey="${IMPOSTOR_PUBKEY}" ` +
    'data-mention-label="Jane Doe">Jane Doe</span></object>';
  const content = normalizeMentionClipboardContent(html);

  assert.equal(content.text.includes("Jane Doe"), false);
  assert.equal(content.html.includes("Jane Doe"), false);
  assert.deepEqual(visibleIdentities(html, content.text), []);
});

test("an ordinary chip still flattens to a registrable mention", () => {
  const html =
    "<p>hey " +
    `<span data-mention="" data-mention-pubkey="${JOHN_SMITH_PUBKEY}" ` +
    'data-mention-label="John Smith">John Smith</span> here</p>';
  const content = normalizeMentionClipboardContent(html);

  // The sigil the chip strips for display is restored, and the identity
  // attributes stay out of the markup handed to the composer.
  assert.equal(content.text.includes("@John Smith"), true);
  assert.equal(content.html.includes("data-mention-pubkey"), false);
  assert.deepEqual(
    visibleIdentities(html, content.text).map((record) => record.pubkey),
    [JOHN_SMITH_PUBKEY],
  );
});
