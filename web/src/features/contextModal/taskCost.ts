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
