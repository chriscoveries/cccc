// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vite-plus/test";
import { SettingsModal } from "./SettingsModal";
import { useModalStore } from "../stores";
import { writeSettingsLastLocation } from "./modals/settings/settingsLastLocation";

// Hold the lazily imported GuidanceTab chunk until the test releases it, so the chunk
// always resolves after the render's act() scope has exited, as on a loaded runner.
const guidanceChunk = vi.hoisted(() => {
  const gate: { release?: () => void } = {};
  const loaded = new Promise<void>((resolve) => {
    gate.release = () => resolve();
  });
  return { loaded, release: () => gate.release?.() };
});
vi.mock("./modals/settings/GuidanceTab", async (importOriginal) => {
  await guidanceChunk.loaded;
  return importOriginal<typeof import("./modals/settings/GuidanceTab")>();
});
// React's warning when a suspended resource resolves while no act() scope is open.
const warnedOutsideAct = (calls: unknown[][]) =>
  calls.some((args) => args.join(" ").includes("suspended resource finished loading"));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
  Trans: ({ i18nKey }: { i18nKey: string }) => i18nKey,
}));
vi.mock("../services/api", async (original) => ({
  ...(await original<typeof import("../services/api")>()),
  fetchWebAccessSession: async () => ({
    ok: true,
    result: { web_access_session: { can_access_global_settings: true } },
  }),
  fetchActors: async () => ({ ok: true, result: { actors: [] } }),
  fetchGroupPrompts: async () => ({
    ok: true,
    result: {
      preamble: {
        kind: "preamble",
        content: "Preamble",
        filename: "preamble.md",
        source: "home",
        path: "/fixture/preamble.md",
      },
      help: {
        kind: "help",
        content: "Help",
        filename: "help.md",
        source: "home",
        path: "/fixture/help.md",
      },
    },
  }),
  fetchObservability: async () => ({
    ok: true,
    result: { observability: { developer_mode: false, log_level: "INFO" } },
  }),
}));

let host: HTMLDivElement;
let root: ReturnType<typeof createRoot>;
beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    callback(0);
    return 1;
  });
  vi.stubGlobal("cancelAnimationFrame", vi.fn());
  HTMLElement.prototype.scrollIntoView = vi.fn();
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  writeSettingsLastLocation({ scope: "group", groupTab: "guidance", globalTab: "account" });
  useModalStore.setState({ settingsTarget: null });
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  window.localStorage.clear();
  useModalStore.setState({ settingsTarget: null });
  vi.unstubAllGlobals();
});

it("fixed structure: delayed Guidance chunk resolves inside act()", async () => {
  const consoleError = vi.spyOn(console, "error");
  const closeSettings = vi.fn();
  await act(async () =>
    root.render(
      <SettingsModal
        codexVoice={{} as never}
        isOpen
        onClose={closeSettings}
        settings={null}
        onUpdateSettings={async () => {}}
        busy={false}
        isDark={false}
        groupId="g"
      />,
    ),
  );
  guidanceChunk.release();
  // Readiness wait as in the fix: back-to-back act() scopes.
  while (host.querySelectorAll("textarea").length !== 2) {
    await act(async () => new Promise((resolve) => setTimeout(resolve, 10)));
  }
  expect(warnedOutsideAct(consoleError.mock.calls)).toBe(false);
  const trigger = [...host.querySelectorAll<HTMLButtonElement>("button")].filter(
    (button) => button.textContent === "guidance.expand",
  )[0];
  trigger.focus();
  await act(async () => trigger.click());
  const child = await vi.waitFor(() => {
    const dialog = [...document.querySelectorAll<HTMLElement>('[role="dialog"]')].find(
      (candidate) => !host.contains(candidate),
    );
    expect(dialog).toBeDefined();
    return dialog!;
  });
  expect(child.contains(document.activeElement)).toBe(true);
});
