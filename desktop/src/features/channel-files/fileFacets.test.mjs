import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_AUTHOR_SORT_KEY_LENGTH,
  MAX_EXTENSION_LENGTH,
  applyFileFacets,
  classifyFile,
  countFacets,
  extensionOf,
} from "./fileFacets.ts";
import { MAX_FILENAME_LENGTH } from "./boundedImeta.ts";

/** A projected row with only the fields the facets read. */
function fileRow(overrides = {}) {
  return {
    key: overrides.key ?? `key-${overrides.filename ?? "row"}`,
    rawUrl: "https://media.example/blob",
    mimeType: "application/octet-stream",
    size: undefined,
    filename: undefined,
    pubkey: "author-1",
    createdAt: 1_000,
    caption: undefined,
    ...overrides,
  };
}

// ── Classification ───────────────────────────────────────────────────────────

test("a .md sent as application/octet-stream is a document", () => {
  assert.equal(
    classifyFile(
      fileRow({ filename: "notes.md", mimeType: "application/octet-stream" }),
    ),
    "document",
  );
});

test("the filename wins over a MIME type that disagrees", () => {
  assert.equal(
    classifyFile(fileRow({ filename: "report.pdf", mimeType: "image/png" })),
    "document",
  );
});

test("every named document extension classifies as a document", () => {
  for (const filename of ["a.md", "a.pdf", "a.html", "a.docx", "a.csv"]) {
    assert.equal(
      classifyFile(fileRow({ filename })),
      "document",
      `${filename} is a document`,
    );
  }
});

test("an uppercase extension classifies the same as a lowercase one", () => {
  assert.equal(classifyFile(fileRow({ filename: "NOTES.MD" })), "document");
  assert.equal(classifyFile(fileRow({ filename: "Sheet.CSV" })), "document");
});

test("an unknown extension falls through to the MIME type", () => {
  assert.equal(
    classifyFile(
      fileRow({ filename: "export.xyz", mimeType: "application/pdf" }),
    ),
    "document",
    "the MIME type still decides when the extension says nothing",
  );
  assert.equal(
    classifyFile(fileRow({ filename: "clip.xyz", mimeType: "video/mp4" })),
    "video",
  );
  assert.equal(
    classifyFile(
      fileRow({ filename: "blob.xyz", mimeType: "application/octet-stream" }),
    ),
    "other",
  );
});

test("a filename with no extension falls through to the MIME type", () => {
  assert.equal(
    classifyFile(fileRow({ filename: "README", mimeType: "text/markdown" })),
    "document",
  );
  assert.equal(
    classifyFile(fileRow({ filename: "README", mimeType: "image/png" })),
    "image",
  );
  assert.equal(
    classifyFile(
      fileRow({ filename: ".md", mimeType: "application/octet-stream" }),
    ),
    "other",
    "a dotfile has no extension, so nothing before the dot names a type",
  );
});

test("a missing filename classifies from the MIME type alone", () => {
  assert.equal(classifyFile(fileRow({ mimeType: "image/webp" })), "image");
  assert.equal(classifyFile(fileRow({})), "other");
});

test("an over-long extension is not examined", () => {
  const atCap = "m".repeat(MAX_EXTENSION_LENGTH);
  assert.equal(extensionOf(`x.${atCap}`), atCap, "the cap itself still reads");
  assert.equal(
    extensionOf(`x.${atCap}m`),
    undefined,
    "one character past the cap, the suffix is not an extension",
  );
  assert.equal(extensionOf("notes.MD"), "md");
  assert.equal(extensionOf("README"), undefined);
  assert.equal(extensionOf(".md"), undefined);
  assert.equal(extensionOf("archive.tar."), undefined);
  assert.equal(extensionOf(undefined), undefined);
  // The guard bounds the suffix it examines, not the classification.
  assert.equal(
    classifyFile(
      fileRow({ filename: "notes.md", mimeType: "application/octet-stream" }),
    ),
    "document",
  );
});

test("a path-like filename reads the extension of the last segment", () => {
  assert.equal(
    classifyFile(
      fileRow({
        filename: "folder.pdf/plain",
        mimeType: "application/octet-stream",
      }),
    ),
    "other",
    "the dot belongs to a directory, not to the file",
  );
});

test("countFacets counts every row once, under the facet it renders in", () => {
  const counts = countFacets([
    fileRow({ key: "1", filename: "a.md" }),
    fileRow({ key: "2", mimeType: "image/png" }),
    fileRow({ key: "3", mimeType: "video/mp4" }),
    fileRow({ key: "4", mimeType: "application/octet-stream" }),
  ]);
  assert.deepEqual(counts, {
    all: 4,
    image: 1,
    video: 1,
    document: 1,
    other: 1,
  });
});

// ── Sort stability ───────────────────────────────────────────────────────────

/**
 * Rows that tie on every sort key except their id, offered in an order that
 * disagrees with the id order, so a comparator that leaves ties to the
 * engine's own ordering fails these.
 */
function tiedRows() {
  return [
    fileRow({ key: "k-c", filename: "same.txt", size: 5, createdAt: 10 }),
    fileRow({ key: "k-a", filename: "same.txt", size: 5, createdAt: 10 }),
    fileRow({ key: "k-b", filename: "same.txt", size: 5, createdAt: 10 }),
  ];
}

for (const sort of ["newest", "oldest", "name", "size", "author"]) {
  test(`sort "${sort}" breaks ties on the entry id`, () => {
    const rows = tiedRows();
    const ordered = applyFileFacets(rows, { sort });
    assert.deepEqual(
      ordered.map((f) => f.key),
      ["k-a", "k-b", "k-c"],
      "ties resolve to id order, whatever the input order",
    );
    // Same input reversed, same output: the order is a function of the rows.
    assert.deepEqual(
      applyFileFacets([...rows].reverse(), { sort }).map((f) => f.key),
      ordered.map((f) => f.key),
    );
  });
}

test("date sorts run newest-first and oldest-first", () => {
  const rows = [
    fileRow({ key: "b", createdAt: 100 }),
    fileRow({ key: "a", createdAt: 300 }),
    fileRow({ key: "c", createdAt: 200 }),
  ];
  assert.deepEqual(
    applyFileFacets(rows, { sort: "newest" }).map((f) => f.key),
    ["a", "c", "b"],
  );
  assert.deepEqual(
    applyFileFacets(rows, { sort: "oldest" }).map((f) => f.key),
    ["b", "c", "a"],
  );
});

test("name sorts on the label the row shows, size sorts largest first", () => {
  const rows = [
    fileRow({ key: "1", filename: "b.txt", size: 10 }),
    fileRow({
      key: "2",
      filename: undefined,
      rawUrl: "https://m.example/a.txt",
      size: 30,
    }),
    fileRow({ key: "3", filename: "c.txt", size: 20 }),
  ];
  assert.deepEqual(
    applyFileFacets(rows, { sort: "name" }).map((f) => f.key),
    ["2", "1", "3"],
    "the unnamed row sorts under the URL tail the list displays",
  );
  assert.deepEqual(
    applyFileFacets(rows, { sort: "size" }).map((f) => f.key),
    ["2", "3", "1"],
  );
});

test("a row with no size sorts after every sized row", () => {
  const rows = [
    fileRow({ key: "none", size: undefined }),
    fileRow({ key: "small", size: 1 }),
  ];
  assert.deepEqual(
    applyFileFacets(rows, { sort: "size" }).map((f) => f.key),
    ["small", "none"],
  );
});

// ── Author sort ──────────────────────────────────────────────────────────────

test("author sort orders by display name, unknown authors last", () => {
  const rows = [
    fileRow({ key: "1", pubkey: "pk-unknown" }),
    fileRow({ key: "2", pubkey: "pk-zoe" }),
    fileRow({ key: "3", pubkey: "pk-ada" }),
  ];
  const authorNames = new Map([
    ["pk-zoe", "Zoe"],
    ["pk-ada", "Ada"],
  ]);
  assert.deepEqual(
    applyFileFacets(rows, { sort: "author", authorNames }).map((f) => f.key),
    ["3", "2", "1"],
  );
});

test("author sort with a missing author is deterministic, not accidental", () => {
  const rows = [
    fileRow({ key: "k-2", pubkey: "pk-b" }),
    fileRow({ key: "k-1", pubkey: "pk-a" }),
    fileRow({ key: "k-3", pubkey: "pk-a" }),
  ];
  // No name map at all: every author is unknown.
  assert.deepEqual(
    applyFileFacets(rows, { sort: "author" }).map((f) => f.key),
    ["k-1", "k-3", "k-2"],
    "unknown authors order by pubkey, then by entry id",
  );
  // A blank name is not a name.
  assert.deepEqual(
    applyFileFacets(rows, {
      sort: "author",
      authorNames: new Map([["pk-b", "   "]]),
    }).map((f) => f.key),
    ["k-1", "k-3", "k-2"],
  );
});

test("author sort caps the relay-sourced display name it compares", () => {
  const shared = "A".repeat(MAX_AUTHOR_SORT_KEY_LENGTH);
  const rows = [
    fileRow({ key: "k-2", pubkey: "pk-long" }),
    fileRow({ key: "k-1", pubkey: "pk-longer" }),
  ];
  const authorNames = new Map([
    // Both names are identical up to the cap and differ only past it, so a
    // comparison that reads the whole string would order them by the suffix.
    ["pk-long", `${shared}zzz`],
    ["pk-longer", `${shared}aaa`],
  ]);
  assert.deepEqual(
    applyFileFacets(rows, { sort: "author", authorNames }).map((f) => f.key),
    ["k-2", "k-1"],
    "past the cap the names tie, so the pubkey decides — not the suffix",
  );
});

test("name sort caps the relay-sourced filename it compares", () => {
  const shared = "a".repeat(MAX_FILENAME_LENGTH);
  const rows = [
    fileRow({ key: "k-1", filename: `${shared}zzz` }),
    fileRow({ key: "k-2", filename: `${shared}aaa` }),
  ];
  assert.deepEqual(
    applyFileFacets(rows, { sort: "name" }).map((f) => f.key),
    ["k-1", "k-2"],
    "past the cap the names tie, so the entry id decides — not the suffix",
  );
});

// ── Filtering ────────────────────────────────────────────────────────────────

test("the Documents facet selects exactly the document rows", () => {
  const rows = [
    fileRow({
      key: "1",
      filename: "notes.md",
      mimeType: "application/octet-stream",
    }),
    fileRow({ key: "2", filename: "photo.png", mimeType: "image/png" }),
    fileRow({ key: "3", filename: "sheet.csv", mimeType: "text/csv" }),
    fileRow({ key: "4", filename: "clip.mp4", mimeType: "video/mp4" }),
  ];
  assert.deepEqual(
    applyFileFacets(rows, { facet: "document", sort: "name" }).map(
      (f) => f.key,
    ),
    ["1", "3"],
  );
  assert.deepEqual(
    applyFileFacets(rows, { facet: "all", sort: "name" }).length,
    4,
  );
});

test("the search query matches the filename and the caption", () => {
  const rows = [
    fileRow({ key: "1", filename: "Quarterly.pdf" }),
    fileRow({ key: "2", filename: "other.pdf", caption: "the QUARTERLY plan" }),
    fileRow({ key: "3", filename: "unrelated.pdf" }),
  ];
  assert.deepEqual(
    applyFileFacets(rows, { query: "quarterly", sort: "name" }).map(
      (f) => f.key,
    ),
    ["2", "1"],
    '"other.pdf" sorts before "Quarterly.pdf"; both matched',
  );
});

test("filtering never hands back the caller's own array", () => {
  const rows = [fileRow({ key: "1" })];
  const result = applyFileFacets(rows, { facet: "all", sort: "newest" });
  assert.notEqual(result, rows, "the input array is never sorted in place");
  assert.deepEqual(
    rows.map((f) => f.key),
    ["1"],
  );
});
