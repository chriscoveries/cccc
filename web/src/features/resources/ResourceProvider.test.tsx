// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vite-plus/test";
import { fetchActors } from "../../services/api/actors";
import { ActorResourcesBadge, HostResourcesBadge, ResourceProvider } from "./ResourceProvider";

vi.mock("../../services/api/actors", () => ({ fetchActors: vi.fn() }));

let host: HTMLDivElement;
let root: ReturnType<typeof createRoot>;
afterEach(async () => {
  await act(async () => root?.unmount());
  root = undefined!;
  host?.remove();
  vi.resetAllMocks();
  vi.useRealTimers();
  Object.defineProperty(document, "hidden", { configurable: true, value: false });
});

function mockOk() {
  vi.mocked(fetchActors).mockResolvedValue({
    ok: true,
    result: { actors: [], host_memory: { total_bytes: 2, available_bytes: 1 } },
  });
}

function setHidden(hidden: boolean) {
  Object.defineProperty(document, "hidden", { configurable: true, value: hidden });
}

async function renderProvider(groupId: string) {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => {
    root.render(
      <ResourceProvider groupId={groupId}>
        <HostResourcesBadge />
        <ActorResourcesBadge actorId="capture" />
      </ResourceProvider>,
    );
  });
}

async function tick(ms: number) {
  await act(async () => vi.advanceTimersByTimeAsync(ms));
}

it("renders and clears the captured process and host readings", async () => {
  // Values derive from the adjacent captured /proc stat, meminfo and CPU pair;
  // the actor key is redacted and the API envelope is a test wrapper.
  const observedCpu = (100 * 120 * 1_000_000_000) / (100 * 1_200_170_447);
  vi.mocked(fetchActors).mockResolvedValue({
    ok: true,
    result: {
      actors: [{ id: "capture", resources: { rss_bytes: 2976 * 4096, cpu_percent: observedCpu } }],
      host_memory: { total_bytes: 32_089_156 * 1024, available_bytes: 16_869_080 * 1024 },
    },
  });
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => {
    root.render(
      <ResourceProvider groupId="capture">
        <HostResourcesBadge />
        <ActorResourcesBadge actorId="capture" />
      </ResourceProvider>,
    );
  });
  expect(host.textContent).toContain("RAM 14.5 GiB / 16.1 GiB");
  expect(host.textContent).toContain("RAM 14.5G/16.1G");
  expect(host.textContent).toContain("12 MiB · 100.0%");
  expect(host.querySelector('[aria-label^="Actor memory"]')?.getAttribute("title")).toContain(
    "shared pages",
  );
  vi.mocked(fetchActors).mockRejectedValue(new Error("unavailable"));
  await act(async () => {
    root.render(
      <ResourceProvider groupId="unavailable">
        <HostResourcesBadge />
        <ActorResourcesBadge actorId="capture" />
      </ResourceProvider>,
    );
  });
  expect(host.textContent).toBe("");
});

it("cancels the polling loop when the group unmounts", async () => {
  vi.useFakeTimers();
  mockOk();
  await renderProvider("capture");
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(1);
  await tick(10_000);
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(2);
  await act(async () => root.unmount());
  root = undefined!;
  await tick(60_000);
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(2);
});

it("stops polling while the tab is hidden and resumes on visibility", async () => {
  vi.useFakeTimers();
  mockOk();
  setHidden(false);
  await renderProvider("capture");
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(1);
  setHidden(true);
  await tick(30_000);
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(1);
  setHidden(false);
  await act(async () => document.dispatchEvent(new Event("visibilitychange")));
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(2);
  await tick(10_000);
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(3);
});

it("makes no request while mounted hidden and fetches once visible", async () => {
  vi.useFakeTimers();
  mockOk();
  setHidden(true);
  await renderProvider("capture");
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(0);
  await tick(30_000);
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(0);
  setHidden(false);
  await act(async () => document.dispatchEvent(new Event("visibilitychange")));
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(1);
});

it("does not start a second request while one is in flight", async () => {
  vi.useFakeTimers();
  let release!: (value: Awaited<ReturnType<typeof fetchActors>>) => void;
  vi.mocked(fetchActors).mockImplementation(
    () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  );
  setHidden(false);
  await renderProvider("capture");
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(1);
  setHidden(true);
  await act(async () => document.dispatchEvent(new Event("visibilitychange")));
  setHidden(false);
  await act(async () => document.dispatchEvent(new Event("visibilitychange")));
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(1);
  await act(async () =>
    release({
      ok: true,
      result: { actors: [], host_memory: { total_bytes: 2, available_bytes: 1 } },
    }),
  );
  await tick(10_000);
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(2);
});

it("stops the previous group's loop when the group changes", async () => {
  vi.useFakeTimers();
  mockOk();
  await renderProvider("alpha");
  expect(vi.mocked(fetchActors).mock.calls[0]?.[0]).toBe("alpha");
  await act(async () => {
    root.render(
      <ResourceProvider groupId="beta">
        <HostResourcesBadge />
        <ActorResourcesBadge actorId="capture" />
      </ResourceProvider>,
    );
  });
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(2);
  expect(vi.mocked(fetchActors).mock.calls[1]?.[0]).toBe("beta");
  await tick(10_000);
  expect(vi.mocked(fetchActors)).toHaveBeenCalledTimes(3);
  expect(vi.mocked(fetchActors).mock.calls[2]?.[0]).toBe("beta");
});
