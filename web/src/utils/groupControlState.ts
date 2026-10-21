import type { Actor } from "../types";
import type { GroupStatusKey } from "./groupStatus";

export type GroupControlState = {
  launchDisabled: boolean;
  pauseDisabled: boolean;
  stopDisabled: boolean;
  launchHardUnavailable: boolean;
  pauseHardUnavailable: boolean;
  stopHardUnavailable: boolean;
  isGroupBusy: boolean;
};

export function resolveGroupControlState(args: {
  selectedGroupId: string;
  actors: Actor[];
  statusKey: GroupStatusKey | null | undefined;
  busy: string;
}): GroupControlState {
  const selectedGroupId = String(args.selectedGroupId || "").trim();
  const actors = Array.isArray(args.actors) ? args.actors : [];
  const statusKey = args.statusKey ?? null;
  const busy = String(args.busy || "");

  const isGroupBusy = busy.startsWith("group-");
  const launchHardUnavailable = !selectedGroupId || actors.length === 0;
  const pauseHardUnavailable = !selectedGroupId || actors.length === 0 || statusKey === "stop";
  const stopHardUnavailable = !selectedGroupId;

  return {
    launchDisabled: launchHardUnavailable || isGroupBusy,
    pauseDisabled: pauseHardUnavailable || isGroupBusy,
    stopDisabled: stopHardUnavailable || isGroupBusy,
    launchHardUnavailable,
    pauseHardUnavailable,
    stopHardUnavailable,
    isGroupBusy,
  };
}
