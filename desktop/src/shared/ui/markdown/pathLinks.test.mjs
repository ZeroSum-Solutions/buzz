import assert from "node:assert/strict";
import { test } from "node:test";

import {
  isPathLinkCandidate,
  localMarkdownDocUrl,
  MAX_PATH_LINK_BYTES,
  parseLocalMarkdownDocUrl,
  parsePathLinkTarget,
  resolvePathLink,
} from "./pathLinks.ts";

/** Records every IPC call so a test can prove one was never made. */
function spyInvoke(result) {
  const calls = [];
  return {
    calls,
    invoke: async (command, payload) => {
      calls.push({ command, payload });
      return result;
    },
  };
}

test("a relative path with a slash is a candidate", () => {
  assert.equal(isPathLinkCandidate("audit/verify/report.md"), true);
  assert.equal(isPathLinkCandidate("buzz/approvals/item-7.html"), true);
});

test("an absolute path is a candidate", () => {
  assert.equal(isPathLinkCandidate("/Users/example/projects/notes.md"), true);
});

test("a slashless name is a candidate only with a document extension", () => {
  assert.equal(isPathLinkCandidate("README.md"), true);
  assert.equal(isPathLinkCandidate("report.pdf"), true);
});

test("bare words never qualify", () => {
  for (const bare of [
    "cargo",
    "true",
    "SIGKILL",
    "npm-run-all",
    "some_value",
    "kind:30078",
  ]) {
    assert.equal(isPathLinkCandidate(bare), false, `${bare} must stay text`);
  }
});

test("an executable-looking name with no slash is not a candidate", () => {
  // `.sh`/`.exe` are not document extensions, so without a slash they never
  // reach the resolver at all.
  assert.equal(isPathLinkCandidate("deploy.sh"), false);
  assert.equal(isPathLinkCandidate("setup.exe"), false);
});

test("a parent-directory segment is refused before any resolution", () => {
  assert.equal(isPathLinkCandidate("../../etc/passwd"), false);
  assert.equal(isPathLinkCandidate("docs/../../../etc/passwd"), false);
  assert.equal(isPathLinkCandidate("/Users/example/../../etc/passwd"), false);
  // A literal `..` inside a filename is not a traversal segment.
  assert.equal(isPathLinkCandidate("docs/two..dots.md"), true);
});

test("a home-relative path is refused (no tilde expansion)", () => {
  assert.equal(isPathLinkCandidate("~/projects/notes.md"), false);
});

test("a URL is never a path candidate", () => {
  assert.equal(isPathLinkCandidate("https://example.com/a/b.md"), false);
  assert.equal(isPathLinkCandidate("buzz://pr/1/2"), false);
  assert.equal(isPathLinkCandidate("file:///etc/passwd"), false);
});

test("whitespace and control characters disqualify a candidate", () => {
  assert.equal(isPathLinkCandidate("docs/a b.md"), false);
  assert.equal(isPathLinkCandidate("docs/a\nb.md"), false);
  assert.equal(isPathLinkCandidate("docs/a\u00a0b.md"), false);
});

test("a candidate over the DTO byte cap is refused", () => {
  const justUnder = `docs/${"a".repeat(MAX_PATH_LINK_BYTES - 9)}.md`;
  assert.equal(
    new TextEncoder().encode(justUnder).length <= MAX_PATH_LINK_BYTES,
    true,
  );
  assert.equal(isPathLinkCandidate(justUnder), true);

  const justOver = `docs/${"a".repeat(MAX_PATH_LINK_BYTES)}.md`;
  assert.equal(isPathLinkCandidate(justOver), false);
});

test("a multi-byte candidate is capped by bytes, not by code units", () => {
  // 2048 code units but 4096 UTF-8 bytes, plus the `docs/` prefix.
  const wide = `docs/${"é".repeat(MAX_PATH_LINK_BYTES / 2)}`;
  assert.equal(wide.length < MAX_PATH_LINK_BYTES, true);
  assert.equal(
    new TextEncoder().encode(wide).length > MAX_PATH_LINK_BYTES,
    true,
  );
  assert.equal(isPathLinkCandidate(wide), false);
});

test("an over-length candidate never reaches the resolver command", async () => {
  const { calls, invoke } = spyInvoke(null);
  const target = await resolvePathLink(
    `docs/${"a".repeat(MAX_PATH_LINK_BYTES)}.md`,
    null,
    invoke,
  );
  assert.equal(target, null);
  assert.deepEqual(calls, []);
});

test("a bare word never reaches the resolver command", async () => {
  const { calls, invoke } = spyInvoke(null);
  assert.equal(await resolvePathLink("cargo", null, invoke), null);
  assert.deepEqual(calls, []);
});

test("a candidate is sent with its sender pubkey", async () => {
  const { calls, invoke } = spyInvoke({
    path: "/Users/example/projects/x/notes.md",
    filename: "notes.md",
    kind: "markdown",
    sizeBytes: 12,
  });
  const target = await resolvePathLink("x/notes.md", "ab12", invoke);
  assert.deepEqual(calls, [
    {
      command: "resolve_path_link",
      payload: { candidate: "x/notes.md", senderPubkey: "ab12" },
    },
  ]);
  assert.deepEqual(target, {
    path: "/Users/example/projects/x/notes.md",
    filename: "notes.md",
    kind: "markdown",
    sizeBytes: 12,
  });
});

test("a path to a missing file stays text", async () => {
  // The command answers `null` for a candidate that does not resolve to a
  // regular file inside a root; that is not an error and renders no link.
  const { invoke } = spyInvoke(null);
  assert.equal(await resolvePathLink("x/missing.md", null, invoke), null);
});

test("a resolver failure propagates instead of rendering a dead link", async () => {
  const invoke = async () => {
    throw new Error("path is too long");
  };
  await assert.rejects(
    () => resolvePathLink("x/notes.md", null, invoke),
    /path is too long/,
  );
});

test("a malformed resolver answer is rejected at the boundary", () => {
  assert.equal(parsePathLinkTarget(undefined), null);
  assert.equal(parsePathLinkTarget({ path: "/a/b.md" }), null);
  assert.equal(
    parsePathLinkTarget({
      path: "/a/b.md",
      filename: "b.md",
      kind: "executable",
      sizeBytes: 1,
    }),
    null,
  );
  assert.deepEqual(
    parsePathLinkTarget({
      path: "/a/b.md",
      filename: "b.md",
      kind: "file",
      sizeBytes: 1,
    }),
    { path: "/a/b.md", filename: "b.md", kind: "file", sizeBytes: 1 },
  );
});

test("the local-document panel URI round-trips the candidate and its sender", () => {
  // The panel re-resolves what the chip resolved: the token as the sender
  // wrote it, under the sender's roots. A canonical path would not survive
  // the trip on Windows (`\\?\C:\...` is refused by inspection), and a
  // null sender would resolve under different roots than the chip did.
  const candidate = "notes and drafts/a b.md";
  const sender = "ab12".repeat(16);
  const url = localMarkdownDocUrl(candidate, sender);
  assert.deepEqual(parseLocalMarkdownDocUrl(url), {
    candidate,
    senderPubkey: sender,
  });
  assert.deepEqual(
    parseLocalMarkdownDocUrl(localMarkdownDocUrl(candidate, null)),
    {
      candidate,
      senderPubkey: null,
    },
  );
  assert.equal(parseLocalMarkdownDocUrl("https://relay/media/a.bin"), null);
  assert.equal(parseLocalMarkdownDocUrl(""), null);
  assert.equal(parseLocalMarkdownDocUrl("buzz-local-file:"), null);
});
