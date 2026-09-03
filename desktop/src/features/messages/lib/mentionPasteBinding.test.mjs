import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { getSchema } from "@tiptap/core";
import { EditorState } from "@tiptap/pm/state";
import StarterKit from "@tiptap/starter-kit";
import { JSDOM } from "jsdom";

import { extractMentionPubkeys } from "./extractMentionPubkeys.ts";
import {
  PastedMentionOccurrencesExtension,
  trackPastedMentionOccurrence,
} from "./pastedMentionOccurrences.ts";

/**
 * The three fences a settled paste has to clear, driven through the hook the
 * composers actually use.
 *
 * Verification is deferred by hand here rather than timed, so each case pins
 * an ordering rather than a race: a paste whose answer is still outstanding, a
 * newer intent for the same label, and an occurrence the user has since
 * deleted. The mention map and `extractMentionPubkeys` are the real ones
 * `useMentions` writes to and reads with, so what these assert is what a send
 * would put in its `p` tags.
 */

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

/** 64-hex, the only shape `parseMentionClipboardRecords` lets through. */
const KEY_A = "a".repeat(64);
const KEY_B = "b".repeat(64);

const PASTED = "@John Smith fixed the bug";
const SECOND_PASTE = " and @John Smith agrees";

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
});

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

after(() => dom.window.close());

const schema = getSchema([
  StarterKit.configure({ heading: false, trailingNode: false, link: false }),
]);
const text = (value) => schema.text(value);
const document_ = (value) =>
  schema.nodes.doc.create(null, [
    schema.nodes.paragraph.create(null, [text(value)]),
  ]);

/** A stand-in for `EditorView` over a real `EditorState` and real plugin. */
function viewWith(initialText) {
  const view = {
    state: EditorState.create({
      doc: document_(initialText),
      schema,
      plugins:
        PastedMentionOccurrencesExtension.config.addProseMirrorPlugins.call({}),
    }),
    dispatch(tr) {
      view.state = view.state.apply(tr);
    },
  };
  return view;
}

/** Clipboard HTML in the shape a Buzz copy writes. */
function clipboardHtml(label, pubkey, body) {
  return (
    '<span data-buzz-copy="markdown">' +
    `<span data-mention="" data-mention-pubkey="${pubkey}" ` +
    `data-mention-label="${label}">@${label}</span>` +
    `${body}</span>`
  );
}

function deferred() {
  let resolve;
  const promise = new Promise((settle) => {
    resolve = settle;
  });
  return { promise, resolve };
}

/**
 * Render the binder with the mention map `useMentions` keeps, and a verifier
 * whose answers the test releases one at a time.
 */
async function renderBinder() {
  const { renderHook } = await import("@testing-library/react");
  const { useMentionPasteBinding } = await import("./mentionPasteBinding.ts");

  /** Stands in for `mentionMapRef.current`; the writer mirrors the hook's. */
  const mentionMap = new Map();
  const answers = [];
  const { result } = renderHook(() =>
    useMentionPasteBinding({
      registerVerifiedMentionPubkey: (displayName, pubkey) => {
        mentionMap.set(displayName.trim(), pubkey);
      },
      verifyMentionIdentities: () => {
        const next = deferred();
        answers.push(next);
        return next.promise;
      },
    }),
  );

  return {
    /** The pubkeys a send would tag for `body`. */
    extract: (body) =>
      extractMentionPubkeys({
        text: body,
        selectedMentions: mentionMap,
        selectedDisplayNames: [],
        memberCandidates: [],
      }),
    mentionMap,
    /** Answer the nth outstanding verification, oldest first. */
    vouch: (index, identities) => answers[index].resolve(identities),
    get binding() {
      return result.current;
    },
  };
}

/** Paste `body`'s records into `view`, tracking the range as production does. */
function paste(binding, view, { label, pubkey, body, from, to }) {
  binding.bindPastedMentionIdentities({
    html: clipboardHtml(label, pubkey, body.slice(`@${label}`.length)),
    insertedText: body,
    occurrenceId: trackPastedMentionOccurrence(view, from, to),
    view,
  });
}

test("a pasted identity binds only once its verification has settled", async () => {
  // The send seams await `settlePendingMentionBindings` precisely because this
  // window exists: sending inside it publishes a readable label with no tag.
  const harness = await renderBinder();
  const view = viewWith(PASTED);
  paste(harness.binding, view, {
    label: "John Smith",
    pubkey: KEY_A,
    body: PASTED,
    from: 1,
    to: 1 + PASTED.length,
  });

  assert.deepEqual(harness.extract(PASTED), [], "nothing binds mid-flight");

  const drained = harness.binding.settlePendingMentionBindings();
  harness.vouch(0, [{ label: "John Smith", pubkey: KEY_A, isAgent: false }]);
  await drained;

  assert.deepEqual(harness.extract(PASTED), [KEY_A]);
});

test("a settled paste does not overwrite a newer paste of the same label", async () => {
  // Slow A, fast B, same label. Both occurrences stay alive, so ordering — not
  // visibility — is the only thing that can decide which pubkey owns the name.
  const harness = await renderBinder();
  const view = viewWith(PASTED + SECOND_PASTE);
  paste(harness.binding, view, {
    label: "John Smith",
    pubkey: KEY_A,
    body: PASTED,
    from: 1,
    to: 1 + PASTED.length,
  });
  paste(harness.binding, view, {
    label: "John Smith",
    pubkey: KEY_B,
    body: SECOND_PASTE,
    from: 1 + PASTED.length,
    to: 1 + PASTED.length + SECOND_PASTE.length,
  });

  harness.vouch(1, [{ label: "John Smith", pubkey: KEY_B, isAgent: false }]);
  harness.vouch(0, [{ label: "John Smith", pubkey: KEY_A, isAgent: false }]);
  await harness.binding.settlePendingMentionBindings();

  assert.deepEqual(harness.extract(PASTED), [KEY_B]);
});

test("an explicit selection outranks a paste still being verified", async () => {
  // What the picker and every other `registerMentionPubkey` caller do: claim
  // the label, then write it. A paste that resolves afterwards is stale.
  const harness = await renderBinder();
  const view = viewWith(PASTED);
  paste(harness.binding, view, {
    label: "John Smith",
    pubkey: KEY_A,
    body: PASTED,
    from: 1,
    to: 1 + PASTED.length,
  });

  harness.binding.claimMentionIntent("John Smith");
  harness.mentionMap.set("John Smith", KEY_B);

  harness.vouch(0, [{ label: "John Smith", pubkey: KEY_A, isAgent: false }]);
  await harness.binding.settlePendingMentionBindings();

  assert.deepEqual(harness.extract(PASTED), [KEY_B]);
});

test("a paste whose text is gone binds nothing, label elsewhere or not", async () => {
  // Delete the paste, then write the same name by hand. "Is this label in the
  // composer?" says yes; the paste no longer owns any of it.
  const harness = await renderBinder();
  const view = viewWith(PASTED);
  paste(harness.binding, view, {
    label: "John Smith",
    pubkey: KEY_A,
    body: PASTED,
    from: 1,
    to: 1 + PASTED.length,
  });

  const pastedEnd = 1 + PASTED.length;
  view.dispatch(view.state.tr.insertText(SECOND_PASTE, pastedEnd));
  view.dispatch(view.state.tr.delete(1, pastedEnd));
  assert.equal(view.state.doc.textContent, SECOND_PASTE);

  harness.vouch(0, [{ label: "John Smith", pubkey: KEY_A, isAgent: false }]);
  await harness.binding.settlePendingMentionBindings();

  assert.deepEqual(harness.extract(SECOND_PASTE), []);
});

test("clearing the composer's mentions retires an in-flight paste", async () => {
  const harness = await renderBinder();
  const view = viewWith(PASTED);
  paste(harness.binding, view, {
    label: "John Smith",
    pubkey: KEY_A,
    body: PASTED,
    from: 1,
    to: 1 + PASTED.length,
  });

  harness.binding.clearMentionIntents();

  harness.vouch(0, [{ label: "John Smith", pubkey: KEY_A, isAgent: false }]);
  await harness.binding.settlePendingMentionBindings();

  assert.deepEqual(harness.extract(PASTED), []);
});

test("a hidden record costs no verification and binds nothing", async () => {
  const harness = await renderBinder();
  const view = viewWith("look at this");
  harness.binding.bindPastedMentionIdentities({
    html:
      `<span data-mention="" data-mention-pubkey="${KEY_A}" ` +
      'data-mention-label="John Smith"></span>look at this',
    insertedText: "look at this",
    occurrenceId: trackPastedMentionOccurrence(
      view,
      1,
      1 + "look at this".length,
    ),
    view,
  });

  // Nothing to settle: the sync visibility gate declined it before any lookup.
  await harness.binding.settlePendingMentionBindings();
  assert.deepEqual(harness.extract(PASTED), []);
});

test("draining is a no-op when nothing is pending", async () => {
  const harness = await renderBinder();
  await harness.binding.settlePendingMentionBindings();
});
