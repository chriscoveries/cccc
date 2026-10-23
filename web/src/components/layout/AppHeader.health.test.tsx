// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vite-plus/test";
import { AppHeader, type AppHeaderProps } from "./AppHeader";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (k: string) => k,
    i18n: { language: "en", changeLanguage: vi.fn() },
  }),
}));
vi.mock("../../services/api/actors", () => ({
  fetchActors: vi.fn(async () => ({
    ok: true,
    result: {
      actors: [
        { id: "actor-a", running: true, rss_mib: 128, cpu_pct: 42 },
        { id: "actor-b", running: true, rss_mib: null, cpu_pct: null },
      ],
    },
  })),
  stopActor: vi.fn(async () => ({ ok: true })),
}));
vi.mock("../../services/api/groups", () => ({
  fetchGroupHealth: vi.fn(async () => ({
    ok: true,
    result: {
      group_id: "g",
      rss_mib: 128,
      pss_mib: 100,
      actors_running: 1,
      actors_sampled: 1,
      host_mem_total_mib: 16384,
      host_mem_available_mib: 9000,
      host_loadavg_1m: 1.2,
      host_loadavg_5m: 1.0,
      host_loadavg_15m: 0.8,
      host_cpu_count: 8,
      sampled_at: "2026-10-22T22:04:48Z",
    },
  })),
}));

const noop = () => {};
const props: AppHeaderProps = {
  theme: "dark",
  textScale: 100,
  onThemeChange: noop,
  onTextScaleChange: noop,
  selectedGroupId: "g",
  groupDoc: { group_id: "g", title: "g", state: "active" },
  selectedGroupRunning: true,
  selectedGroupRuntimeStatus: null,
  sseStatus: "connected",
  onOpenSidebar: noop,
  onOpenSearch: noop,
  onOpenContext: noop,
  onOpenSettings: noop,
  canAccessAccount: false,
  onOpenAccount: noop,
  onOpenMobileMenu: noop,
  onControlGroup: noop,
};

let host: HTMLDivElement;
let root: ReturnType<typeof createRoot>;
afterEach(async () => {
  await act(async () => root?.unmount());
  host?.remove();
  vi.clearAllMocks();
});

async function mount() {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  vi.stubGlobal(
    "confirm",
    vi.fn(() => true),
  );
  await act(async () => root.render(<AppHeader {...props} />));
  await new Promise((r) => setTimeout(r, 50));
  await act(async () => {});
}

it("renders actor rows and calls stopActor", async () => {
  await mount();
  const btn = host.querySelector<HTMLButtonElement>("[data-group-run-control]")!;
  await act(async () => {
    btn.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerType: "mouse" }));
    btn.dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 }));
  });
  await act(async () => {});
  const menu = document.querySelector<HTMLElement>("[role='menu']");
  expect(menu).toBeTruthy();
  expect(menu!.textContent).toContain("actor-a");
  expect(menu!.textContent).toContain("128 MiB");
  expect(menu!.textContent).toContain("42%");
  expect(menu!.textContent).toContain("actor-b");
  expect(menu!.textContent).toContain("—");
  const buttons = Array.from(menu!.querySelectorAll<HTMLButtonElement>("button"));
  const target = buttons.find((b) => b.textContent?.includes("actor-a"));
  expect(target).toBeTruthy();
  await act(async () => target!.click());
  const { stopActor } = await import("../../services/api/actors");
  expect(stopActor).toHaveBeenCalledWith("g", "actor-a");
});
