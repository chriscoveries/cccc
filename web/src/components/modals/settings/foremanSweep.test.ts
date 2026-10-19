import { describe, expect, it } from "vite-plus/test";
import { withForemanSweep } from "./foremanSweep";
import type { AutomationRuleSet } from "../../../types";

describe("foreman sweep opt-in", () => {
  it("creates one hourly rule addressed to the foreman without touching other rules or snippets", () => {
    const input: AutomationRuleSet = {
      rules: [{ id: "other", enabled: true }],
      snippets: { local: "keep" },
    };
    const result = withForemanSweep(input, true);
    expect(input.rules).toHaveLength(1);
    expect(result.rules[0]).toBe(input.rules[0]);
    expect(result.snippets).toBe(input.snippets);
    expect(result.rules[1]).toMatchObject({
      id: "foreman-scan",
      enabled: true,
      to: ["@foreman"],
      trigger: { kind: "interval", every_seconds: 3600 },
    });
    expect(withForemanSweep(result, false).rules).toHaveLength(2);
    expect(withForemanSweep(result, false).rules[1].enabled).toBe(false);
  });
  it("toggles an existing custom cron and message without replacing it", () => {
    const input: AutomationRuleSet = {
      rules: [
        {
          id: "foreman-scan",
          enabled: false,
          to: ["@foreman"],
          trigger: { kind: "cron", cron: "0 * * * *", timezone: "UTC" },
          action: { kind: "notify", message: "custom" },
        },
      ],
      snippets: {},
    };
    expect(withForemanSweep(input, true).rules[0]).toEqual({ ...input.rules[0], enabled: true });
  });
});
