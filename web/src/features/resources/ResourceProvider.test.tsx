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
  host?.remove();
  vi.resetAllMocks();
});

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
