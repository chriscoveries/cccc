// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vite-plus/test";
import type { HarnessRamReport } from "../../types";
import { HarnessRamTable } from "./HarnessRamTable";
import { formatMemoryMib, sparklinePoints } from "./harnessRamFormat";

const mocks = vi.hoisted(() => ({
  report: null as HarnessRamReport | null,
  calls: 0,
  fail: false,
  deferred: false,
  response: null as unknown,
  settle: null as ((value: unknown) => void) | null,
}));

vi.mock("../../services/api/actors", () => ({
  fetchMemoryReport: () => {
    mocks.calls += 1;
    const response = mocks.fail
      ? { ok: false, error: { code: "unavailable", message: "daemon offline" } }
      : { ok: true, result: mocks.report };
    mocks.response = response;
    if (!mocks.deferred) return Promise.resolve(response);
    return new Promise((resolve) => {
      mocks.settle = resolve;
    });
  },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (_key: string, options?: Record<string, unknown>) => String(options?.defaultValue || _key),
  }),
}));

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  mocks.calls = 0;
  mocks.fail = false;
  mocks.deferred = false;
  mocks.response = null;
  mocks.settle = null;
  mocks.report = {
    window_hours: 72,
    bucket_seconds: 3600,
    as_of: "2026-10-22T21:00:00Z",
    keys: [
      {
        harness: "codex",
        mode: "headless",
        peak_rss_mib: 1342,
        avg_rss_mib: 892,
        samples: 240,
        peak_pss_mib: 610,
        avg_pss_mib: 402,
        pss_samples: 240,
        hours_observed: 3,
        series: [
          { hour: "2026-10-22T19:00:00Z", peak_rss_mib: 1100, avg_rss_mib: 700, samples: 80 },
          { hour: "2026-10-22T20:00:00Z", peak_rss_mib: 1342, avg_rss_mib: 892, samples: 80 },
          { hour: "2026-10-22T21:00:00Z", peak_rss_mib: 900, avg_rss_mib: 600, samples: 80 },
        ],
      },
    ],
  };
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

it("fetches once when opened and shows the harness cost", async () => {
  await act(async () => {
    root.render(<HarnessRamTable groupId="g_test" isDark={false} />);
  });
  expect(mocks.calls).toBe(0);
  expect(host.textContent).toContain("RAM");

  const toggle = host.querySelector("button");
  await act(async () => {
    toggle?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await Promise.resolve();
  });

  expect(mocks.calls).toBe(1);
  const text = host.textContent || "";
  expect(text).toContain("Harness RAM, 72 h");
  expect(text).toContain("codex");
  expect(text).toContain("headless");
  expect(text).toContain("1.3G");
  expect(text).toContain("892M");
  expect(text).toContain("240");

  const row = host.querySelector("tbody tr");
  expect(row?.getAttribute("title")).toBe("codex headless: peak 1.3G, average 892M over 3h");

  await act(async () => {
    toggle?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    toggle?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await Promise.resolve();
  });
  expect(mocks.calls).toBe(1);
});

// Regression: the panel survives a Group switch, so the report it held was the
// previous Group's - shown as if it were the new Group's until the new answer
// landed. A switch now starts from nothing.
it("drops the previous group's rows when the group changes", async () => {
  await act(async () => {
    root.render(<HarnessRamTable groupId="g_one" isDark={false} />);
  });
  await act(async () => {
    host.querySelector("button")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await Promise.resolve();
  });
  expect(host.textContent).toContain("codex");

  mocks.deferred = true;
  mocks.report = {
    ...mocks.report!,
    keys: [{ ...mocks.report!.keys[0], harness: "claude", peak_rss_mib: 512, avg_rss_mib: 400 }],
  };
  await act(async () => {
    root.render(<HarnessRamTable groupId="g_two" isDark={false} />);
  });
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });

  const switched = host.textContent || "";
  expect(switched).not.toContain("codex");
  expect(switched).toContain("Loading");

  await act(async () => {
    mocks.settle?.(mocks.response);
  });
  expect(host.textContent).toContain("claude");
});

// Regression: the request used to be cancelled by the re-render its own
// "loading" state caused, so a slow answer never landed and the panel sat on
// "Loading" for as long as the fetch took.
it("keeps the answer when it arrives after the render it causes", async () => {
  mocks.deferred = true;
  await act(async () => {
    root.render(<HarnessRamTable groupId="g_slow" isDark={false} />);
  });
  await act(async () => {
    host.querySelector("button")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
  expect(mocks.calls).toBe(1);
  expect(host.textContent).toContain("Loading");

  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
  await act(async () => {
    mocks.settle?.(mocks.response);
  });

  const text = host.textContent || "";
  expect(text).not.toContain("Loading");
  expect(text).toContain("codex");
  expect(text).toContain("1.3G");
});

it("offers a retry when the request fails", async () => {
  mocks.fail = true;
  await act(async () => {
    root.render(<HarnessRamTable groupId="g_retry" isDark={false} />);
  });
  await act(async () => {
    host.querySelector("button")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
  expect(host.textContent).toContain("daemon offline");
  expect(mocks.calls).toBe(1);

  mocks.fail = false;
  const retry = Array.from(host.querySelectorAll("button")).find(
    (button) => button.textContent === "Retry",
  );
  await act(async () => {
    retry?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });

  expect(mocks.calls).toBe(2);
  expect(host.textContent).toContain("codex");
});

it("says when there is nothing yet, and shows a load error", async () => {
  mocks.report = {
    window_hours: 72,
    bucket_seconds: 3600,
    as_of: "2026-10-22T21:00:00Z",
    keys: [],
  };
  await act(async () => {
    root.render(<HarnessRamTable groupId="g_empty" isDark />);
    await Promise.resolve();
  });
  await act(async () => {
    host.querySelector("button")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await Promise.resolve();
    await Promise.resolve();
  });
  expect(host.textContent).toContain("No readings in the last 72 hours yet.");

  mocks.fail = true;
  await act(async () => {
    root.unmount();
  });
  root = createRoot(host);
  await act(async () => {
    root.render(<HarnessRamTable groupId="g_error" isDark />);
    await Promise.resolve();
  });
  await act(async () => {
    host.querySelector("button")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await Promise.resolve();
    await Promise.resolve();
  });
  expect(host.textContent).toContain("daemon offline");
});

it("formats MiB the way an operator reads it", () => {
  expect(formatMemoryMib(null)).toBe("—");
  expect(formatMemoryMib(0)).toBe("0M");
  expect(formatMemoryMib(336)).toBe("336M");
  expect(formatMemoryMib(999)).toBe("999M");
  expect(formatMemoryMib(1024)).toBe("1.0G");
  expect(formatMemoryMib(1342)).toBe("1.3G");
});

it("draws one point per observed hour, tallest at the peak hour", () => {
  const points = sparklinePoints([
    { avg_rss_mib: 100 },
    { avg_rss_mib: 200 },
    { avg_rss_mib: 50 },
  ]).split(" ");
  expect(points).toHaveLength(3);
  const y = points.map((point) => Number(point.split(",")[1]));
  expect(y[1]).toBe(0);
  expect(y[0]).toBeGreaterThan(y[1]);
  expect(y[2]).toBeGreaterThan(y[1]);
  expect(sparklinePoints([])).toBe("");
  expect(sparklinePoints([{ avg_rss_mib: null }])).toBe("0.0,16.0");
});
