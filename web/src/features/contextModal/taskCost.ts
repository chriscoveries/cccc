import type { Task } from "../../types";

/** `$0.0123` — four decimals while a task is cheap, two once it is not. */
export function formatUsd(usd: number): string {
  if (!Number.isFinite(usd)) return "$-";
  if (Math.abs(usd) >= 1) return `$${usd.toFixed(2)}`;
  return `$${usd.toFixed(4)}`;
}

/**
 * What a task has cost, in the words the task detail shows.
 *
 * The spend is the metered figure the daemon recorded; turns it could not price
 * (no price for the model) and turns that reported no tokens are counted beside
 * it rather than folded into it, so nobody reads a complete total out of a
 * partial one.
 */
export function taskSpendLabel(task: Task): string {
  const spent = typeof task.cost_usd === "number" ? task.cost_usd : null;
  if (spent === null) return "-";
  const parts = [formatUsd(spent)];
  if (task.target_budget_usd != null) parts.push(`of ${formatUsd(task.target_budget_usd)}`);
  const unpriced = task.unpriced_turns ?? 0;
  if (unpriced > 0) parts.push(`+${unpriced} unpriced`);
  const unmetered = task.unmetered_turns ?? 0;
  if (unmetered > 0) parts.push(`+${unmetered} unmetered`);
  return parts.join(" ");
}

/** `$0.07/Mtok`, or `-` when the task sets no ceiling on model price. */
export function taskPriceCeilingLabel(task: Task): string {
  const ceiling = task.max_usd_per_mtok;
  if (ceiling == null) return "-";
  return `$${ceiling}/Mtok`;
}

/**
 * What a budget field shows for a stored value: empty when the field is unset.
 *
 * The stored figure is a float, so its raw text can carry noise nobody typed
 * (a real stage readback stored 0.004972450000000001). Dollars are shown to
 * eight decimals, which is far finer than any price, and anything smaller than
 * that is shown as it is rather than being rounded away to zero.
 */
export function budgetFieldText(value: number | null | undefined): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return "";
  const rounded = Number(value.toFixed(8));
  if (Number.isFinite(rounded) && (rounded === value || rounded !== 0)) return String(rounded);
  return String(value);
}

export type BudgetField = { ok: true; value: number | null } | { ok: false };

/**
 * One budget field's text as the value to store.
 *
 * Empty clears the field rather than writing zero, because zero is a real value
 * (a free model, a budget of nothing) and clearing is not. Anything that is not
 * a plain non-negative number is refused instead of being coerced.
 */
export function parseBudgetField(text: string): BudgetField {
  const trimmed = String(text ?? "").trim();
  if (!trimmed) return { ok: true, value: null };
  const parsed = Number(trimmed);
  if (!Number.isFinite(parsed) || parsed < 0) return { ok: false };
  return { ok: true, value: parsed };
}

export type TaskBudgetUpdate =
  | { ok: true; fields: { target_budget_usd: number | null; max_usd_per_mtok: number | null } }
  | { ok: false; field: "target_budget_usd" | "max_usd_per_mtok" };

/**
 * The budget half of a task update, from the editor's two fields.
 *
 * Both fields always travel: the daemon reads an absent budget as "leave it
 * alone", so a cleared input has to be sent as an explicit null to clear.
 */
export function taskBudgetUpdate(draft: {
  targetBudgetUsd: string;
  maxUsdPerMtok: string;
}): TaskBudgetUpdate {
  const target = parseBudgetField(draft.targetBudgetUsd);
  if (!target.ok) return { ok: false, field: "target_budget_usd" };
  const ceiling = parseBudgetField(draft.maxUsdPerMtok);
  if (!ceiling.ok) return { ok: false, field: "max_usd_per_mtok" };
  return { ok: true, fields: { target_budget_usd: target.value, max_usd_per_mtok: ceiling.value } };
}
