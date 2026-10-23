import { describe, expect, it } from "vitest";
import type { Task } from "../../types";
import {
  budgetFieldText,
  formatUsd,
  parseBudgetField,
  taskBudgetUpdate,
  taskPriceCeilingLabel,
  taskSpendLabel,
} from "./taskCost";

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

describe("task budget fields", () => {
  // Values as the stage's final readback reported them: T094 carried 0.07 and
  // 0.07, T095 was given 0.00497245 after the helper wrote it.
  it("shows the stored budget and price, and nothing when they are unset", () => {
    expect(budgetFieldText(0.07)).toBe("0.07");
    // The stage's readback carried float noise; the field shows the amount.
    expect(budgetFieldText(0.004972450000000001)).toBe("0.00497245");
    expect(budgetFieldText(1e-9)).toBe("1e-9");
    expect(budgetFieldText(0)).toBe("0");
    expect(budgetFieldText(null)).toBe("");
    expect(budgetFieldText(undefined)).toBe("");
  });

  it("clears on an empty field and keeps a real zero", () => {
    expect(parseBudgetField("")).toEqual({ ok: true, value: null });
    expect(parseBudgetField("   ")).toEqual({ ok: true, value: null });
    expect(parseBudgetField("0")).toEqual({ ok: true, value: 0 });
    expect(parseBudgetField(" 0.00497245 ")).toEqual({ ok: true, value: 0.00497245 });
  });

  it("refuses anything that is not a plain non-negative number", () => {
    for (const text of ["-1", "abc", "$0.07", "0,07", "1e999"]) {
      expect(parseBudgetField(text)).toEqual({ ok: false });
    }
  });

  it("sends both fields, so a cleared one clears rather than being left alone", () => {
    expect(taskBudgetUpdate({ targetBudgetUsd: "0.07", maxUsdPerMtok: "0.07" })).toEqual({
      ok: true,
      fields: { target_budget_usd: 0.07, max_usd_per_mtok: 0.07 },
    });
    expect(taskBudgetUpdate({ targetBudgetUsd: "", maxUsdPerMtok: "" })).toEqual({
      ok: true,
      fields: { target_budget_usd: null, max_usd_per_mtok: null },
    });
    expect(taskBudgetUpdate({ targetBudgetUsd: "0.00497245", maxUsdPerMtok: "" })).toEqual({
      ok: true,
      fields: { target_budget_usd: 0.00497245, max_usd_per_mtok: null },
    });
  });

  it("names the field that was refused", () => {
    expect(taskBudgetUpdate({ targetBudgetUsd: "-2", maxUsdPerMtok: "0.07" })).toEqual({
      ok: false,
      field: "target_budget_usd",
    });
    expect(taskBudgetUpdate({ targetBudgetUsd: "0.07", maxUsdPerMtok: "cheap" })).toEqual({
      ok: false,
      field: "max_usd_per_mtok",
    });
  });
});
