// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vite-plus/test";
const mocks = vi.hoisted(() => ({ load: vi.fn(), update: vi.fn() }));
vi.mock("../../services/api", () => ({
  fetchHarkActorState: (...a: unknown[]) => mocks.load(...a),
  updateHarkActorConnection: (...a: unknown[]) => mocks.update(...a),
}));
import { HarkActorPanel } from "./HarkActorPanel";
let host: HTMLDivElement;
let root: Root;
const state = (extra = {}) => ({
  ok: true,
  result: {
    group_id: "g_test",
    actor_id: "hark",
    enabled: true,
    configured: false,
    phase: "unconfigured",
    pending_count: 0,
    ...extra,
  },
});
beforeEach(() => {
  vi.clearAllMocks();
  mocks.load.mockResolvedValue(state());
  mocks.update.mockResolvedValue(
    state({
      configured: true,
      secret: "synthetic-secret",
      connector: {
        connector_url: "/mcp/web-model/test",
        connector_url_path_token: "/private/synthetic",
      },
    }),
  );
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});
async function render(props = {}) {
  await act(async () => root.render(<HarkActorPanel groupId="g_test" actorId="hark" {...props} />));
}
async function click(text: string) {
  const button = [...host.querySelectorAll("button")].find((b) => b.textContent === text);
  expect(button).toBeTruthy();
  await act(async () => button?.click());
}
it("creates once, hides the credential, and requires a separate confirmation to rotate", async () => {
  await render();
  await click("Create Hark connector");
  expect(mocks.update).toHaveBeenCalledWith("g_test", "hark", "create", undefined, undefined);
  expect([...host.querySelectorAll("input")].some((i) => i.value === "synthetic-secret")).toBe(
    true,
  );
  await click("Hide credential");
  expect(host.querySelector("input")).toBeNull();
  await click("Rotate credential");
  expect(mocks.update).toHaveBeenCalledTimes(1);
  await click("Cancel");
  expect(mocks.update).toHaveBeenCalledTimes(1);
  await click("Rotate credential");
  await click("Confirm rotate");
  expect(mocks.update).toHaveBeenCalledTimes(2);
});
it("does not expose a late create credential in another actor or hidden panel", async () => {
  let resolve!: (value: ReturnType<typeof state>) => void;
  mocks.update.mockImplementation(
    () =>
      new Promise((r) => {
        resolve = r;
      }),
  );
  await render();
  await click("Create Hark connector");
  await render({ actorId: "other" });
  await act(async () => resolve(state({ configured: true, secret: "stale-secret" })));
  expect([...host.querySelectorAll("input")].some((i) => i.value === "stale-secret")).toBe(false);
  await render({ isVisible: false });
  expect(host.textContent).toBe("");
});
it("resolves only the displayed unfinished turn after confirmation", async () => {
  mocks.load.mockResolvedValue(
    state({
      configured: true,
      phase: "interrupted",
      active_turn_id: "turn-1",
      active_event_ids: ["event-1"],
    }),
  );
  await render();
  await click("Resolve unfinished turn");
  expect(mocks.update).not.toHaveBeenCalled();
  await click("Confirm resolve");
  expect(mocks.update).toHaveBeenCalledWith("g_test", "hark", "resolve", "turn-1", ["event-1"]);
});
