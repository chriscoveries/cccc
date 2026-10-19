// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vite-plus/test";
import { AppFeedback } from "./AppFeedback";

const mocks = vi.hoisted(() => ({ request: vi.fn() }));
vi.mock("../../services/api/base", () => ({ apiJson: mocks.request }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: { defaultValue?: string }) => options?.defaultValue ?? key,
  }),
}));
let host: HTMLDivElement, root: ReturnType<typeof createRoot>;
const health = (severity: string, available = 7 * 1024 ** 3) => ({
  ok: true,
  result: {
    disk: {
      scope: "cccc_home",
      severity,
      available_bytes: available,
      total_bytes: 100 * 1024 ** 3,
      used_percent: 86,
    },
  },
});
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  vi.useFakeTimers();
  mocks.request.mockReset();
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.useRealTimers();
});
async function render(readOnly = false) {
  await act(async () =>
    root.render(
      <AppFeedback
        isDark={false}
        webReadOnly={readOnly}
        errorMsg=""
        notice={null}
        dismissError={() => {}}
        dismissNotice={() => {}}
      />,
    ),
  );
}
it("shows home-volume pressure through the real application feedback surface", async () => {
  mocks.request.mockResolvedValue(health("warning"));
  await render();
  expect(host.querySelector('[role="alert"]')).not.toBeNull();
  expect(host.textContent).toContain("7.0 GiB");
  expect(mocks.request).toHaveBeenCalledWith("/api/v1/health");
});
it("keeps the storage warning visible in a read-only workbench", async () => {
  mocks.request.mockResolvedValue(health("critical", 2 * 1024 ** 3));
  await render(true);
  expect(host.querySelector('[role="alert"]')).not.toBeNull();
  expect(host.textContent).toContain("2.0 GiB");
});
it("rechecks pressure and removes the banner after recovery", async () => {
  mocks.request.mockResolvedValue(health("warning"));
  await render();
  expect(host.querySelector('[role="alert"]')).not.toBeNull();
  mocks.request.mockResolvedValue(health("normal", 30 * 1024 ** 3));
  await act(async () => {
    await vi.advanceTimersByTimeAsync(30_000);
  });
  expect(host.querySelector('[role="alert"]')).toBeNull();
});
it("does not invent a warning for older or unauthenticated health responses", async () => {
  mocks.request.mockResolvedValue({ ok: true, result: { status: "ok" } });
  await render();
  expect(host.querySelector('[role="alert"]')).toBeNull();
});

it("shows unknown storage capacity without claiming a healthy sample", async () => {
  mocks.request.mockResolvedValue({
    ok: true,
    result: { disk: { scope: "cccc_home", severity: "unknown" } },
  });
  await render();
  expect(host.querySelector('[role="alert"]')).not.toBeNull();
  expect(host.textContent).toContain("capacity is unavailable");
  expect(host.textContent).not.toContain("GiB");
});
