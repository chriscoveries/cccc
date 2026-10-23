// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it } from "vite-plus/test";
import type { Task } from "../../../types";
import { evaluateTaskWorkflow, getTaskTypeDefinition } from "../../../utils/taskWorkflow";
import { taskToDraft, type TaskDraft } from "../model";
import { createContextModalUi } from "../ui";
import { TaskEditorPanel } from "./TaskEditorPanel";
import { taskBudgetUpdate } from "../../../features/contextModal/taskCost";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

// The stage's final readback for T094, exactly as the API returned it: a real
// delivered card with a budget, a model-price ceiling and one priced turn.
const stageTask = {
  id: "T094",
  title: "Verify installed Pi package metadata",
  status: "planned",
  assignee: "stage-peer-c",
  task_type: "standard",
  target_budget_usd: 0.07,
  max_usd_per_mtok: 0.07,
  cost_usd: 0.00773752,
  cost_turns: 1,
  unpriced_turns: 0,
  unmetered_turns: 0,
} as Task;

let host: HTMLDivElement;
let root: Root;
let draft: TaskDraft;

function render() {
  const workflow = evaluateTaskWorkflow({
    status: draft.status,
    taskType: draft.taskType,
    assignee: draft.assignee,
    outcome: draft.outcome,
    notes: draft.notes,
    checklist: draft.checklist,
  });
  const setTaskDraft = (
    next: TaskDraft | null | ((current: TaskDraft | null) => TaskDraft | null),
  ) => {
    const resolved = typeof next === "function" ? next(draft) : next;
    if (!resolved) throw new Error("the editor cleared its own draft");
    draft = resolved;
    act(() => render());
  };
  act(() =>
    root.render(
      <TaskEditorPanel
        tr={(key, fallback) => fallback || key}
        ui={createContextModalUi(false)}
        taskEditorMode="edit"
        taskDraft={draft}
        hasTaskUnsaved={false}
        syncBusy={false}
        selectedTask={stageTask}
        selectedTaskDeleteInfo={{ allowed: true, total: 1, reason: "" }}
        selectedTaskDeleteHint=""
        taskWorkflowCoverage={workflow}
        taskTypeId="standard"
        selectedTaskType={getTaskTypeDefinition("standard")}
        setTaskDraft={setTaskDraft}
        onTaskTypeChange={() => {}}
        onResetTask={() => {}}
        onClose={() => {}}
        onDeleteSelectedTask={() => {}}
        onSaveTask={() => {}}
      />,
    ),
  );
}

function field(label: string): HTMLInputElement {
  const input = host.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
  if (!input) throw new Error(`no field labelled ${label}`);
  return input;
}

function type(input: HTMLInputElement, value: string) {
  act(() => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

beforeEach(() => {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  draft = taskToDraft(stageTask);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

it("shows the task's stored budget and model price, and its spend as read-only", () => {
  render();
  expect(field("Target budget (USD)").value).toBe("0.07");
  expect(field("Max model price ($/Mtok)").value).toBe("0.07");
  expect(host.textContent).toContain("$0.0077 of $0.0700");
  expect(host.textContent).toContain("Spend is recorded by the daemon and cannot be edited here.");
});

it("saves an edited budget through the normal task update", () => {
  render();
  type(field("Target budget (USD)"), "0.00497245");
  expect(taskBudgetUpdate(draft)).toEqual({
    ok: true,
    fields: { target_budget_usd: 0.00497245, max_usd_per_mtok: 0.07 },
  });
});

it("clears a budget by emptying its field, without touching the other one", () => {
  render();
  type(field("Target budget (USD)"), "");
  expect(draft.targetBudgetUsd).toBe("");
  expect(taskBudgetUpdate(draft)).toEqual({
    ok: true,
    fields: { target_budget_usd: null, max_usd_per_mtok: 0.07 },
  });

  type(field("Max model price ($/Mtok)"), "");
  expect(taskBudgetUpdate(draft)).toEqual({
    ok: true,
    fields: { target_budget_usd: null, max_usd_per_mtok: null },
  });
});

it("keeps a typed zero a real value rather than a cleared field", () => {
  render();
  type(field("Max model price ($/Mtok)"), "0");
  expect(taskBudgetUpdate(draft)).toEqual({
    ok: true,
    fields: { target_budget_usd: 0.07, max_usd_per_mtok: 0 },
  });
});

it("refuses a budget that is not a number instead of coercing it", () => {
  render();
  type(field("Target budget (USD)"), "-1");
  expect(taskBudgetUpdate(draft)).toEqual({ ok: false, field: "target_budget_usd" });
});
