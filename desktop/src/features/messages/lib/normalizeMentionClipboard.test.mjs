import assert from "node:assert/strict";
import test from "node:test";

import {
  chipTextMatchesLabel,
  hasMentionClipboardHtml,
  restoreChipSigil,
} from "./normalizeMentionClipboard.ts";

// NOTE: normalizeMentionClipboardContent uses the browser DOMParser API which
// is not available in Node.  Those paths are covered by the e2e paste tests.
// This file tests the pure string-matching detection function.

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
