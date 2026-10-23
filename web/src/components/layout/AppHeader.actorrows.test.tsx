// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vite-plus/test";
import { AppHeader, type AppHeaderProps } from "./AppHeader";
import { fetchActors, stopActor } from "../../services/api/actors";

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

/** The real readback from the trial at claw/cost-stack@1df17d47. */
function trialActors() {
  return {
    actors: [
      {
        id: "stage-foreman",
        runtime: "hermes",
        model: "free",
        model_cost_usd_per_mtok: 0.0,
        model_strength: 2,
        running: true,
        rss_mib: 31,
        cpu_pct: null,
      },
      {
        id: "stage-peer-c",
        runtime: "pi",
        model: "ds1017",
        model_cost_usd_per_mtok: 0.07,
        model_strength: 3,
        running: true,
        rss_mib: null,
        cpu_pct: null,
      },
      {
        id: "stage-peer-a",
        runtime: "hermes",
        model_cost_usd_per_mtok: null,
        model_strength: null,
        running: true,
        rss_mib: 31,
        cpu_pct: null,
      },
      { id: "stage-seat", runtime: "hermes", running: false, rss_mib: null, cpu_pct: null },
    ],
  };
}

afterEach(async () => {
  await act(async () => root?.unmount());
  host?.remove();
  vi.clearAllMocks();
});

it("renders the price and strength from the actor API, with a dash when null", async () => {
  vi.mocked(fetchActors).mockResolvedValue({ ok: true, result: trialActors() } as never);
  await mount();
  await act(async () =>
    host
      .querySelector<HTMLButtonElement>("[data-group-run-controls]")!
      .dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 })),
  );
  const menu = document.querySelector<HTMLElement>('[role="menu"]')!;
  const items = Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]'));
  const labels = items.map((item) => item.textContent ?? "");
  expect(
    labels.some(
      (label) =>
        label.includes("stage-foreman") && label.includes("$0/Mtok") && label.includes("str 2"),
    ),
  ).toBe(true);
  expect(
    labels.some(
      (label) =>
        label.includes("stage-peer-c") && label.includes("$0.07/Mtok") && label.includes("str 3"),
    ),
  ).toBe(true);
  expect(labels.some((label) => label.includes("stage-peer-a") && label.includes("—"))).toBe(true);
  expect(labels.some((label) => label.includes("stage-seat"))).toBe(false);
});

it("stops a running actor from the row", async () => {
  vi.mocked(fetchActors).mockResolvedValue({ ok: true, result: trialActors() } as never);
  vi.mocked(stopActor).mockResolvedValue({ ok: true } as never);
  await mount();
  await act(async () =>
    host
      .querySelector<HTMLButtonElement>("[data-group-run-controls]")!
      .dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 })),
  );
  const menu = document.querySelector<HTMLElement>('[role="menu"]')!;
  const row = Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]')).find((item) =>
    (item.textContent ?? "").includes("stage-peer-c"),
  )!;
  await act(async () => row.dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 })));
  expect(stopActor).toHaveBeenCalledWith("group-1", "stage-peer-c");
});
