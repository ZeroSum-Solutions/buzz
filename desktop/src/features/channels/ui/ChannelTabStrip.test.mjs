import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    Element: dom.window.Element,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    Node: dom.window.Node,
    window: dom.window,
  });
});

after(() => dom.window.close());

async function renderStrip(initialTab = "chat") {
  const { act, cleanup, fireEvent, render, screen } = await import(
    "@testing-library/react"
  );
  const React = await import("react");
  const { ChannelTabStrip, channelTabPanelId } = await import(
    "./ChannelTabStrip.tsx"
  );

  let current = initialTab;
  function Harness() {
    const [tab, setTab] = React.useState(initialTab);
    current = tab;
    return React.createElement(ChannelTabStrip, {
      activeTab: tab,
      onSelect: setTab,
    });
  }

  await act(async () => {
    render(React.createElement(Harness));
  });

  return {
    act,
    channelTabPanelId,
    cleanup,
    fireEvent,
    screen,
    get activeTab() {
      return current;
    },
  };
}

test("the tab strip is one Tab stop with a roving tabIndex", async () => {
  const strip = await renderStrip();
  try {
    const chat = strip.screen.getByRole("tab", { name: "Chat" });
    const files = strip.screen.getByRole("tab", { name: "Files" });
    assert.equal(chat.tabIndex, 0);
    assert.equal(files.tabIndex, -1, "the inactive tab is not a Tab stop");
    assert.equal(chat.getAttribute("aria-selected"), "true");
    assert.equal(
      files.getAttribute("aria-controls"),
      strip.channelTabPanelId("files"),
      "each tab names the panel it owns",
    );
  } finally {
    strip.cleanup();
  }
});

test("ArrowRight, ArrowLeft, Home and End move between tabs", async () => {
  const strip = await renderStrip();
  try {
    const chat = strip.screen.getByRole("tab", { name: "Chat" });

    await strip.act(async () => {
      strip.fireEvent.keyDown(chat, { key: "ArrowRight" });
    });
    assert.equal(strip.activeTab, "files");
    assert.equal(
      strip.screen.getByRole("tab", { name: "Files" }).tabIndex,
      0,
      "focus and the roving tabIndex follow the selection",
    );

    await strip.act(async () => {
      strip.fireEvent.keyDown(
        strip.screen.getByRole("tab", { name: "Files" }),
        {
          key: "ArrowLeft",
        },
      );
    });
    assert.equal(strip.activeTab, "chat");

    await strip.act(async () => {
      strip.fireEvent.keyDown(strip.screen.getByRole("tab", { name: "Chat" }), {
        key: "End",
      });
    });
    assert.equal(strip.activeTab, "files");

    await strip.act(async () => {
      strip.fireEvent.keyDown(
        strip.screen.getByRole("tab", { name: "Files" }),
        {
          key: "Home",
        },
      );
    });
    assert.equal(strip.activeTab, "chat");
  } finally {
    strip.cleanup();
  }
});

test("ArrowRight wraps around from the last tab", async () => {
  const strip = await renderStrip("files");
  try {
    await strip.act(async () => {
      strip.fireEvent.keyDown(
        strip.screen.getByRole("tab", { name: "Files" }),
        {
          key: "ArrowRight",
        },
      );
    });
    assert.equal(strip.activeTab, "chat");
  } finally {
    strip.cleanup();
  }
});
