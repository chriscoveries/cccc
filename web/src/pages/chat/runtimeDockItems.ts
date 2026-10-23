import { isBrowserWebModelRuntime, type Actor } from "../../types";
import { getEffectiveActorRunner } from "../../utils/headlessRuntimeSupport";
import type { LiveWorkCard } from "./liveWorkCards";

export type RuntimeDockRunner = "pty" | "headless";

export type RuntimeDockItem = {
  actor: Actor;
  actorId: string;
  actorLabel: string;
  runtime: string;
  runner: RuntimeDockRunner;
  unreadCount: number;
  webModelQueuedCount: number;
  memoryMib: number | null;
  memoryLabel: string;
  liveWorkCard: LiveWorkCard | null;
};

function getRuntimeDockRunner(actor: Actor): RuntimeDockRunner {
  return getEffectiveActorRunner(actor);
}

/** What the actor's process tree holds, for the dock's tooltip and status line. */
function buildMemoryLabel(actor: Actor): string {
  const rss = actor.rss_mib;
  if (typeof rss !== "number") return "";
  const parts = [`${rss} MiB RSS`];
  if (typeof actor.pss_mib === "number") parts.push(`${actor.pss_mib} MiB PSS`);
  if (typeof actor.n_proc === "number" && actor.n_proc > 1) parts.push(`${actor.n_proc} procs`);
  return parts.join(", ");
}

export function buildRuntimeDockItems(args: {
  actors: Actor[];
  liveWorkCards: LiveWorkCard[];
}): RuntimeDockItem[] {
  const liveWorkCardByActorId = new Map<string, LiveWorkCard>();
  for (const card of Array.isArray(args.liveWorkCards) ? args.liveWorkCards : []) {
    const actorId = String(card.actorId || "").trim();
    if (!actorId || liveWorkCardByActorId.has(actorId)) continue;
    liveWorkCardByActorId.set(actorId, card);
  }

  const items: RuntimeDockItem[] = [];
  for (const actor of Array.isArray(args.actors) ? args.actors : []) {
    const actorId = String(actor.id || "").trim();
    if (!actorId) continue;
    items.push({
      actor,
      actorId,
      actorLabel: String(actor.title || actorId).trim() || actorId,
      runtime: String(actor.runtime || "custom").trim() || "custom",
      runner: getRuntimeDockRunner(actor),
      unreadCount: Math.max(0, Number(actor.unread_count || 0)),
      webModelQueuedCount: isBrowserWebModelRuntime(actor.runtime)
        ? Math.max(0, Number(actor.web_model_queued_count || 0))
        : 0,
      memoryMib: actor.rss_mib ?? null,
      memoryLabel: buildMemoryLabel(actor),
      liveWorkCard: liveWorkCardByActorId.get(actorId) || null,
    });
  }
  return items;
}
