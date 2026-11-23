import { useEffect, useRef, useState } from "react";
import * as api from "../../services/api";
import type { HarkActorState } from "../../services/api/webModel";
import { Button } from "../ui/button";

type Props = {
  groupId: string;
  actorId: string;
  isVisible?: boolean;
  canAccessConnection?: boolean;
  saving?: boolean;
  onBusyChange?: (busy: boolean) => void;
};

export function HarkActorPanel(props: Props) {
  return (
    <HarkActorPanelScope
      key={`${props.groupId}/${props.actorId}/${props.isVisible !== false}`}
      {...props}
    />
  );
}

function HarkActorPanelScope({
  groupId,
  actorId,
  isVisible = true,
  canAccessConnection = true,
  saving = false,
  onBusyChange,
}: Props) {
  const [state, setState] = useState<HarkActorState | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [confirmation, setConfirmation] = useState<"rotate" | "revoke" | "resolve" | null>(null);
  const live = useRef(false);
  const version = useRef(0);
  const writing = useRef(false);
  const busyCallback = useRef(onBusyChange);
  busyCallback.current = onBusyChange;

  useEffect(() => {
    live.current = true;
    ++version.current;
    const finishBusy = busyCallback.current;
    if (!isVisible || !canAccessConnection)
      return () => {
        live.current = false;
      };
    const refresh = async () => {
      if (writing.current) return;
      const request = ++version.current;
      try {
        const response = await api.fetchHarkActorState(groupId, actorId);
        if (!live.current || request !== version.current) return;
        if (response.ok) {
          setState((previous) => ({
            ...response.result,
            ...(previous?.secret ? { secret: previous.secret, connector: previous.connector } : {}),
          }));
          setError("");
        } else setError(response.error.message);
      } catch {
        if (live.current && request === version.current)
          setError("Could not read Hark connection status.");
      }
    };
    void refresh();
    const timer = window.setInterval(() => {
      void refresh();
    }, 5000);
    return () => {
      live.current = false;
      window.clearInterval(timer);
      finishBusy?.(false);
    };
  }, [groupId, actorId, isVisible, canAccessConnection]);

  async function change(action: "create" | "rotate" | "revoke" | "resolve") {
    if (writing.current || saving || !state || !isVisible || !canAccessConnection) return;
    writing.current = true;
    ++version.current;
    setBusy(true);
    busyCallback.current?.(true);
    setError("");
    setConfirmation(null);
    try {
      const response = await api.updateHarkActorConnection(
        groupId,
        actorId,
        action,
        action === "resolve" ? (state.active_turn_id ?? undefined) : undefined,
        action === "resolve" ? state.active_event_ids : undefined,
      );
      if (!live.current) return;
      if (response.ok) setState(response.result);
      else setError(response.error.message);
    } catch {
      if (live.current)
        setError("The request could not be confirmed. Refresh status before trying again.");
    } finally {
      writing.current = false;
      if (live.current) {
        setBusy(false);
        busyCallback.current?.(false);
      }
    }
  }

  if (!isVisible) return null;
  if (!canAccessConnection) return <p>Owner access is required to manage the Hark connector.</p>;
  const locked = busy || saving || !state;
  return (
    <section aria-label="Hark connector" className="space-y-3">
      <h3 className="font-medium">Hark (pull)</h3>
      <p className="text-xs text-[var(--color-text-muted)]">
        This connector belongs only to this Group and actor. Save its credential in Hark’s vault,
        never in chat. Schedule one poll every 10 minutes; no inbound URL or ChatGPT pairing is
        needed.
      </p>
      {error && (
        <p role="alert" className="text-sm text-red-500">
          {error}
        </p>
      )}
      <div className="text-xs space-y-1">
        <div>State: {state?.phase ?? "Loading"}</div>
        <div>
          Last poll: {state?.last_poll_at ?? "Never"} {state?.last_poll_status ?? ""}
        </div>
        <div>Last claim: {state?.last_claim_at ?? "Never"}</div>
        <div>Reason: {state?.reason_code ?? state?.error_code ?? "None"}</div>
        <div>Pending: {state?.pending_count ?? 0}</div>
      </div>
      {state?.secret && (
        <div className="space-y-2 rounded border p-3">
          <p className="text-sm font-medium">Shown once. Store in Hark’s vault before closing.</p>
          <label className="block text-xs">
            MCP URL
            <input
              readOnly
              value={state.connector?.connector_url ?? ""}
              className="block w-full font-mono"
            />
          </label>
          <label className="block text-xs">
            Authorization: Bearer credential
            <input
              readOnly
              type="password"
              value={state.secret}
              className="block w-full font-mono"
            />
          </label>
          <label className="block text-xs">
            Private URL alternative
            <input
              readOnly
              type="password"
              value={state.connector?.connector_url_path_token ?? ""}
              className="block w-full font-mono"
            />
          </label>
          <Button
            type="button"
            variant="outline"
            onClick={() =>
              setState((previous) =>
                previous
                  ? {
                      ...previous,
                      secret: undefined,
                      connector: previous.connector
                        ? { ...previous.connector, connector_url_path_token: undefined }
                        : undefined,
                    }
                  : previous,
              )
            }
          >
            Hide credential
          </Button>
        </div>
      )}
      <p className="text-xs">
        Poll cccc_runtime_wait_next_turn with an empty object. Use its returned actor_token for
        replies, named-actor sends and cccc_runtime_complete_turn. Idle, stopped or turn_in_progress
        means exit. Workspace execution is unavailable.
      </p>
      <div className="flex flex-wrap gap-2">
        {!state?.configured ? (
          <Button type="button" disabled={locked} onClick={() => void change("create")}>
            Create Hark connector
          </Button>
        ) : (
          <>
            <Button
              type="button"
              variant="outline"
              disabled={locked}
              onClick={() => setConfirmation("rotate")}
            >
              Rotate credential
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={locked}
              onClick={() => setConfirmation("revoke")}
            >
              Revoke connector
            </Button>
          </>
        )}
        {state?.active_turn_id && (
          <Button
            type="button"
            variant="outline"
            disabled={locked}
            onClick={() => setConfirmation("resolve")}
          >
            Resolve unfinished turn
          </Button>
        )}
      </div>
      {state?.active_turn_id && (
        <p className="text-xs">
          Unfinished turn: {state.active_turn_id}. Work will not be replayed automatically.
        </p>
      )}
      {confirmation && (
        <div
          role="alertdialog"
          aria-label="Confirm Hark connection change"
          className="space-y-2 rounded border p-3"
        >
          <p className="text-sm">
            {confirmation === "resolve"
              ? "Resolve only after checking the external work. This closes the unfinished turn without replaying it."
              : "This revokes the old connector and current execution credentials. Cloud work already running is not stopped and unfinished work will not be replayed."}
          </p>
          <Button type="button" disabled={locked} onClick={() => void change(confirmation)}>
            Confirm {confirmation}
          </Button>
          <Button
            type="button"
            variant="ghost"
            disabled={busy}
            onClick={() => setConfirmation(null)}
          >
            Cancel
          </Button>
        </div>
      )}
    </section>
  );
}
