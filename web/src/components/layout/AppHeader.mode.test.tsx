// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vite-plus/test";
import { AppHeader, type AppHeaderProps } from "./AppHeader";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    i18n: { language: "en", resolvedLanguage: "en", changeLanguage: vi.fn() },
  }),
}));

vi.mock("../../services/api/actors", () => ({ fetchActors: vi.fn(), stopActor: vi.fn() }));

const noop = () => undefined;
const props: AppHeaderProps = {
  theme: "light",
  textScale: 100,
  onThemeChange: noop,
  onTextScaleChange: noop,
  selectedGroupId: "group-1",
  groupDoc: null,
  selectedGroupRunning: true,
  selectedGroupRuntimeStatus: null,
  sseStatus: "connected",
  onOpenSidebar: noop,
  onOpenGroupEdit: noop,
  onOpenSearch: noop,
  onOpenContext: noop,
  onControlGroup: noop,
  onOpenSettings: noop,
  canAccessAccount: false,
  onOpenAccount: noop,
  onOpenMobileMenu: noop,
};
let root: Root;
let host: HTMLDivElement;

async function mount(overrides: Partial<AppHeaderProps> = {}) {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.stubGlobal(
    "confirm",
    vi.fn(() => true),
  );
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => root.render(<AppHeader {...props} {...overrides} />));
}

function capturedActors() {
  return {
    actors: [
      {
        id: "stage-foreman",
        runtime: "hermes",
        runtime_mode: null,
        effective_runtime_mode: "default",
        runner_effective: "pty",
        model: "free",
        model_cost_usd_per_mtok: 0,
        model_strength: 2,
        running: true,
        rss_mib: 338,
        cpu_pct: 0.5,
      },
      {
        id: "stage-peer-c",
        runtime: "pi",
        runtime_mode: null,
        effective_runtime_mode: "default",
        runner_effective: "headless",
        model: "ds1017",
        model_cost_usd_per_mtok: 0.07,
        model_strength: 3,
        running: true,
        rss_mib: 109,
        cpu_pct: 0,
      },
    ],
  };
}

afterEach(async () => {
  await act(async () => root?.unmount());
  host?.remove();
  vi.clearAllMocks();
});

it("shows runner_effective for the mode label when runtime_mode is null", async () => {
  const { fetchActors } = await import("../../services/api/actors");
  vi.mocked(fetchActors).mockResolvedValue({ ok: true, result: capturedActors() } as never);
  await mount();
  await act(async () =>
    host
      .querySelector<HTMLButtonElement>("[data-group-run-controls]")!
      .dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 })),
  );
  const menu = document.querySelector<HTMLElement>('[role="menu"]')!;
  const items = Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]'));
  const labels = items.map((item) => item.textContent ?? "");
  expect(labels.some((label) => label.includes("stage-foreman") && label.includes("pty"))).toBe(
    true,
  );
  expect(labels.some((label) => label.includes("stage-peer-c") && label.includes("headless"))).toBe(
    true,
  );
});
