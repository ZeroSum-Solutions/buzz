import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
  pretendToBeVisual: true,
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    Element: dom.window.Element,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    Node: dom.window.Node,
    ResizeObserver: class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
    getComputedStyle: dom.window.getComputedStyle.bind(dom.window),
    window: dom.window,
  });
  globalThis.window.ResizeObserver = globalThis.ResizeObserver;
});

after(() => dom.window.close());

const EVENT_ID = "e".repeat(64);
const FOLDER_ID = "a".repeat(32);

async function renderTab(props = {}) {
  const { act, cleanup, render, screen } = await import(
    "@testing-library/react"
  );
  const React = await import("react");
  const { ChannelFilesTab } = await import("./ChannelFilesTab.tsx");
  const { fileKeyFor } = await import("./folderStore.ts");

  const files = (props.files ?? []).map((file) => ({
    key: fileKeyFor(file.eventId ?? EVENT_ID, file.url),
    url: file.url,
    rawUrl: file.url,
    mimeType: "image/png",
    size: 10,
    filename: file.filename,
    sha256: undefined,
    thumb: undefined,
    dim: undefined,
    blurhash: undefined,
    pubkey: "sender",
    createdAt: 1_000,
    eventId: file.eventId ?? EVENT_ID,
    caption: undefined,
  }));

  await act(async () => {
    render(
      React.createElement(ChannelFilesTab, {
        canMutateFolders: true,
        isLoading: false,
        onJumpToMessage: () => {},
        ...props,
        files,
      }),
    );
  });

  return { act, cleanup, files, screen };
}

test("selection addresses one attachment, not every attachment on its message", async () => {
  const { act, cleanup, files, screen } = await renderTab({
    files: [
      { url: "https://media.example/one.png", filename: "one.png" },
      { url: "https://media.example/two.png", filename: "two.png" },
    ],
  });
  try {
    assert.notEqual(files[0].key, files[1].key);
    const { fireEvent } = await import("@testing-library/react");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Select" }));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("checkbox", { name: "Select one.png" }));
    });

    assert.ok(screen.getByText("1 selected"), "one row, one selection");
    assert.equal(
      screen
        .getByRole("checkbox", { name: "Select two.png" })
        .getAttribute("aria-checked"),
      "false",
      "the sibling attachment on the same message stays unselected",
    );
  } finally {
    cleanup();
  }
});

test("Shift extends the selection through the keyboard path", async () => {
  const { act, cleanup, screen } = await renderTab({
    files: [
      { url: "https://media.example/a.png", filename: "a.png" },
      { url: "https://media.example/b.png", filename: "b.png" },
      { url: "https://media.example/c.png", filename: "c.png" },
    ],
  });
  try {
    const { fireEvent } = await import("@testing-library/react");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Select" }));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("checkbox", { name: "Select a.png" }));
    });
    await act(async () => {
      fireEvent.keyDown(
        screen.getByRole("checkbox", { name: "Select c.png" }),
        { key: "Enter", shiftKey: true },
      );
    });

    assert.ok(
      screen.getByText("3 selected"),
      "Shift+Enter selects the whole range, not just the focused row",
    );
  } finally {
    cleanup();
  }
});

test("a folder mutation in flight disables its control instead of stacking writes", async () => {
  let resolveAssign;
  const calls = [];
  const { act, cleanup, screen } = await renderTab({
    files: [{ url: "https://media.example/a.png", filename: "a.png" }],
    snapshot: {
      folders: [{ id: FOLDER_ID, name: "Inbox", parent: null }],
      files: {},
    },
    fileFolderMap: new Map(),
    onDeleteFolder: (id) => {
      calls.push(id);
      return new Promise((resolve) => {
        resolveAssign = resolve;
      });
    },
  });
  try {
    const { fireEvent } = await import("@testing-library/react");
    const button = screen.getByRole("button", { name: "Delete folder Inbox" });

    await act(async () => {
      fireEvent.click(button);
    });
    assert.equal(button.hasAttribute("disabled"), true);

    await act(async () => {
      fireEvent.click(button);
    });
    assert.deepEqual(
      calls,
      [FOLDER_ID],
      "a second click while pending is a no-op",
    );

    await act(async () => {
      resolveAssign(undefined);
    });
    assert.equal(button.hasAttribute("disabled"), false);
  } finally {
    cleanup();
  }
});

test("a long list virtualizes instead of mounting every row", async () => {
  const many = Array.from({ length: 200 }, (_, index) => ({
    url: `https://media.example/f-${index}.png`,
    filename: `f-${index}.png`,
    eventId: index.toString(16).padStart(64, "0"),
  }));
  const { cleanup, screen } = await renderTab({ files: many });
  try {
    const mounted = screen
      .getByTestId("channel-files-list")
      .querySelectorAll("a[title]").length;
    assert.ok(
      mounted < many.length,
      `expected a virtualized window, got ${mounted} of ${many.length} rows mounted`,
    );
  } finally {
    cleanup();
  }
});

test("a short list mounts every row so find-in-page still works", async () => {
  const few = Array.from({ length: 5 }, (_, index) => ({
    url: `https://media.example/s-${index}.png`,
    filename: `s-${index}.png`,
    eventId: index.toString(16).padStart(64, "0"),
  }));
  const { cleanup, screen } = await renderTab({ files: few });
  try {
    assert.equal(
      screen.getByTestId("channel-files-list").querySelectorAll("a[title]")
        .length,
      few.length,
      "every row's filename link is in the document",
    );
  } finally {
    cleanup();
  }
});

test("row actions reveal on keyboard focus, not only on hover", async () => {
  const { cleanup, screen } = await renderTab({
    files: [{ url: "https://media.example/a.png", filename: "a.png" }],
  });
  try {
    const jump = screen.getByRole("button", { name: "Jump to message" });
    const cluster = jump.parentElement;
    assert.ok(
      cluster.className.includes("group-focus-within:opacity-100"),
      "an opacity-0 cluster focus can reach but the eye cannot find is unreachable",
    );
  } finally {
    cleanup();
  }
});

test("a failed file load renders a retryable error, never 'No files yet'", async () => {
  let retried = 0;
  const { act, cleanup, screen } = await renderTab({
    files: [],
    isError: true,
    onRetryFiles: () => {
      retried += 1;
    },
  });
  try {
    const { fireEvent } = await import("@testing-library/react");
    assert.equal(screen.queryByText("No files yet"), null);
    assert.ok(screen.getByText("Files could not be loaded"));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    });
    assert.equal(retried, 1);
  } finally {
    cleanup();
  }
});

test("an unreadable folder payload disables every folder mutation control", async () => {
  const { cleanup, screen } = await renderTab({
    files: [{ url: "https://media.example/a.png", filename: "a.png" }],
    canMutateFolders: false,
    foldersInvalidReason: "too-many-folders",
    snapshot: {
      folders: [{ id: FOLDER_ID, name: "Inbox", parent: null }],
      files: {},
    },
    fileFolderMap: new Map(),
    onCreateFolder: async () => undefined,
    onDeleteFolder: async () => undefined,
    onMoveFolder: async () => undefined,
  });
  try {
    assert.ok(screen.getByRole("alert"), "the broken state is surfaced");
    assert.equal(
      screen.getByRole("button", { name: /New/ }).hasAttribute("disabled"),
      true,
    );
    assert.equal(
      screen
        .getByRole("button", { name: "Delete folder Inbox" })
        .hasAttribute("disabled"),
      true,
    );
    assert.equal(
      screen
        .getByRole("combobox", { name: "Move folder Inbox to" })
        .hasAttribute("disabled"),
      true,
    );
  } finally {
    cleanup();
  }
});

test("a folder row exposes its disclosure state and a keyboard move affordance", async () => {
  const moves = [];
  const { act, cleanup, screen } = await renderTab({
    files: [],
    snapshot: {
      folders: [
        { id: FOLDER_ID, name: "Inbox", parent: null },
        { id: "b".repeat(32), name: "Archive", parent: null },
      ],
      files: {},
    },
    fileFolderMap: new Map(),
    onMoveFolder: async (id, parent) => {
      moves.push([id, parent]);
    },
  });
  try {
    const { fireEvent } = await import("@testing-library/react");
    const disclosure = screen.getByRole("button", { name: /Inbox/ });
    assert.equal(disclosure.getAttribute("aria-expanded"), "false");
    await act(async () => {
      fireEvent.click(disclosure);
    });
    assert.equal(disclosure.getAttribute("aria-expanded"), "true");

    const select = screen.getByRole("combobox", {
      name: "Move folder Inbox to",
    });
    assert.ok(
      [...select.options].some((option) => option.value === "root"),
      "Root is reachable without a pointer",
    );
    await act(async () => {
      fireEvent.change(select, { target: { value: "b".repeat(32) } });
    });
    assert.deepEqual(moves, [[FOLDER_ID, "b".repeat(32)]]);
  } finally {
    cleanup();
  }
});

/**
 * A stand-in for the browser's `DataTransfer`, which jsdom does not implement.
 * The drop path reads back exactly what the drag start wrote, so the stub only
 * has to carry the key/value pairs between the two events.
 */
function stubDataTransfer() {
  const store = new Map();
  return {
    dropEffect: "",
    effectAllowed: "",
    getData: (type) => store.get(type) ?? "",
    setData: (type, value) => {
      store.set(type, String(value));
    },
  };
}

test("dragging a file row onto a folder row assigns that one file to the folder", async () => {
  const assignments = [];
  const { act, cleanup, files, screen } = await renderTab({
    files: [{ url: "https://media.example/a.png", filename: "a.png" }],
    snapshot: {
      folders: [{ id: FOLDER_ID, name: "Inbox", parent: null }],
      files: {},
    },
    fileFolderMap: new Map(),
    onAssignFiles: async (keys, folderId) => {
      assignments.push([keys, folderId]);
      return true;
    },
    onMoveFolder: async () => {},
  });
  try {
    const { fireEvent } = await import("@testing-library/react");
    const fileRow = screen
      .getByRole("link", { name: "a.png", exact: true })
      .closest('[draggable="true"]');
    const folderRow = screen
      .getByRole("button", { name: /Inbox/ })
      .closest('[draggable="true"]');
    assert.ok(fileRow, "the file row is draggable");
    assert.ok(folderRow, "the folder row is a drop target");

    const dataTransfer = stubDataTransfer();
    await act(async () => {
      fireEvent.dragStart(fileRow, { dataTransfer });
    });
    assert.equal(
      dataTransfer.getData("text/plain"),
      files[0].key,
      "the drag carries the attachment key, not the message id",
    );

    await act(async () => {
      fireEvent.dragOver(folderRow, { dataTransfer });
    });
    await act(async () => {
      fireEvent.drop(folderRow, { dataTransfer });
    });
    assert.deepEqual(assignments, [[[files[0].key], FOLDER_ID]]);
  } finally {
    cleanup();
  }
});
