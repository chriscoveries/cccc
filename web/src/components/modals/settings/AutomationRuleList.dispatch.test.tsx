// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vite-plus/test";
import { AutomationRuleList } from "./AutomationRuleList";

vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

it("shows task dispatch as a disabled, editable recurring action", async () => {
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const toggle = vi.fn();
  const edit = vi.fn();
  try {
    await act(async () =>
      root.render(
        <AutomationRuleList
          isDark={false}
          visibleRules={[
            {
              id: "dispatch",
              enabled: false,
              scope: "group",
              to: [],
              trigger: { kind: "interval", every_seconds: 60 },
              action: { kind: "dispatch_ready_task", dispatch_cooldown_ms: 60000 },
            },
          ]}
          status={{}}
          rulesBusy={false}
          showCompletedRules={false}
          completedOneTimeRuleIds={[]}
          onToggleShowCompleted={() => {}}
          onClearCompleted={() => {}}
          onToggleRuleEnabled={toggle}
          onEditRule={edit}
          onDeleteRule={() => {}}
        />,
      ),
    );
    expect(host.textContent).toContain("ruleList.dispatchReadyTasks");
    expect(host.textContent).toContain("ruleList.scheduleEveryMinutes");
    expect(host.textContent).not.toContain("ruleList.reminderAction");
    const enabled = host.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')[1]!;
    expect(enabled.checked).toBe(false);
    await act(async () => enabled.click());
    expect(toggle).toHaveBeenCalledWith("dispatch", true);
    const button = [...host.querySelectorAll("button")].find(
      (item) => item.textContent === "common:edit",
    )!;
    await act(async () => button.click());
    expect(edit).toHaveBeenCalledWith("dispatch");
  } finally {
    await act(async () => root.unmount());
    host.remove();
  }
});
