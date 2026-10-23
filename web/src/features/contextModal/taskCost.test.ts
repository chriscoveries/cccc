import { describe, expect, it } from "vitest";
import type { Task } from "../../types";
import { formatUsd, taskPriceCeilingLabel, taskSpendLabel } from "./taskCost";

function task(fields: Partial<Task>): Task {
  return { id: "T001", ...fields };
}

describe("task cost", () => {
  it("shows the metered spend against the budget the task was given", () => {
    // 214_522 tokens at $0.07 per million, as the daemon records it.
    const label = taskSpendLabel(
      task({ cost_usd: 0.01501654, target_budget_usd: 5, cost_turns: 1 }),
    );
    expect(label).toBe("$0.0150 of $5.00");
  });

  it("keeps unpriced turns beside the spend instead of folding them in", () => {
    const label = taskSpendLabel(task({ cost_usd: 0.01501654, unpriced_turns: 2 }));
    expect(label).toBe("$0.0150 +2 unpriced");
  });

  it("says nothing was spent for a task with no budget and no cost", () => {
    expect(taskSpendLabel(task({ cost_usd: 0 }))).toBe("$0.0000");
    expect(taskSpendLabel(task({}))).toBe("-");
  });

  it("names the model price ceiling, or nothing when there is none", () => {
    expect(taskPriceCeilingLabel(task({ max_usd_per_mtok: 0.07 }))).toBe("$0.07/Mtok");
    expect(taskPriceCeilingLabel(task({}))).toBe("-");
    expect(taskPriceCeilingLabel(task({ max_usd_per_mtok: 0 }))).toBe("$0/Mtok");
  });

  it("formats cents once a task is no longer cheap", () => {
    expect(formatUsd(1.239)).toBe("$1.24");
    expect(formatUsd(0.0009)).toBe("$0.0009");
  });
});
