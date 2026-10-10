import type { AutomationRuleSet } from "../../../types";

export function withForemanSweep(ruleset: AutomationRuleSet, enabled: boolean): AutomationRuleSet {
  const existing = ruleset.rules.find((rule) => rule.id === "foreman-scan");
  const rule = existing
    ? { ...existing, enabled }
    : {
        id: "foreman-scan",
        enabled,
        scope: "group" as const,
        owner_actor_id: null,
        to: ["@foreman"],
        trigger: { kind: "interval" as const, every_seconds: 3600 },
        action: {
          kind: "notify" as const,
          title: "Foreman hourly card sweep",
          message:
            "Review this group's cards, pending replies, stalled work and idle agents. Assign ready work and report only decisions or blockers; stay silent when clean.",
          priority: "normal" as const,
        },
      };
  return {
    ...ruleset,
    rules: existing
      ? ruleset.rules.map((item) => (item.id === "foreman-scan" ? rule : item))
      : [...ruleset.rules, rule],
  };
}
