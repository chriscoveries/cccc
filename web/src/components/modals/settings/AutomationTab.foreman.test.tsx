// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vite-plus/test";
import { AutomationTab } from "./AutomationTab";
import * as api from "../../../services/api";
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
vi.mock("../../../services/api", () => ({ fetchAutomation: vi.fn(), updateAutomation: vi.fn() }));

describe("foreman sweep production wiring", () => {
  let host: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;
  afterEach(async () => {
    await act(async () => root.unmount());
    host.remove();
    vi.clearAllMocks();
  });
  it("persists the opt-in with version protection and retains the other rule", async () => {
    const other = {
      id: "other",
      enabled: true,
      scope: "group" as const,
      to: ["@foreman"],
      trigger: { kind: "interval" as const, every_seconds: 900 },
      action: { kind: "notify" as const, message: "keep" },
    };
    vi.mocked(api.fetchAutomation).mockResolvedValue({
      ok: true,
      result: {
        ruleset: { rules: [other], snippets: {} },
        status: {},
        config_path: "fixture",
        supported_vars: [],
        version: 7,
      },
    });
    vi.mocked(api.updateAutomation).mockResolvedValue({ ok: true, result: {} });
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    const set = vi.fn();
    await act(async () =>
      root.render(
        <AutomationTab
          isDark={false}
          groupId="g1"
          devActors={[]}
          busy={false}
          idleSeconds={0}
          setIdleSeconds={set}
          keepaliveSeconds={120}
          setKeepaliveSeconds={set}
          keepaliveMax={3}
          setKeepaliveMax={set}
          silenceSeconds={0}
          setSilenceSeconds={set}
          helpNudgeIntervalSeconds={600}
          setHelpNudgeIntervalSeconds={set}
          helpNudgeMinMessages={10}
          setHelpNudgeMinMessages={set}
          onSavePolicies={set}
          onResetPolicies={set}
        />,
      ),
    );
    const label = Array.from(host.querySelectorAll("label")).find((item) =>
      item.textContent?.includes("automation.foremanSweep"),
    )!;
    const input = label.querySelector<HTMLInputElement>("input")!;
    await vi.waitFor(() => expect(input.disabled).toBe(false));
    await act(async () => input.click());
    expect(api.updateAutomation).toHaveBeenCalledWith(
      "g1",
      expect.objectContaining({
        rules: [
          expect.objectContaining({ id: "other", action: other.action }),
          expect.objectContaining({ id: "foreman-scan", enabled: true, to: ["@foreman"] }),
        ],
      }),
      7,
    );
  });
});
