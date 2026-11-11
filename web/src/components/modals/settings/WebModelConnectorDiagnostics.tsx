import { useTranslation } from "react-i18next";
import type { WebModelConnector } from "../../../services/api";
import { formatMessageTimestamp } from "../../../utils/time";
import { SettingsCard } from "./SettingsKit";
import { secondaryButtonClass } from "./types";

function reasonHint(reason: string | null): string {
  switch (reason) {
    case "pairing_expired":
    case "pairing_invalid":
    case "pairing_failed":
    case "pairing_not_received":
      return "pairingExpired";
    case "pairing_changed":
    case "pairing_already_used":
    case "binding_conflict":
    case "dots_binding_conflict":
    case "dots_binding_changed":
    case "dots_binding_unavailable":
    case "dots_pairing_changed":
    case "paired_actor_unavailable":
    case "paired_actor_identity_changed":
    case "paired_actor_runtime_changed":
      return "bindingChanged";
    case "conversation_not_paired":
    case "conversation_pairing_changed":
    case "session_missing":
    case "session_metadata_invalid":
      return "conversationUnpaired";
    case "dots_token_invalid":
    case "dots_execution_token_required":
      return "executionToken";
    case "dots_actor_stopped":
    case "paired_actor_stopped":
      return "actorStopped";
    case "rpc_-32601":
    case "rpc_-32602":
      return "refreshTools";
    default:
      return "callFailed";
  }
}

/** Safe bounded outcomes from the connector; this is not an Actor task dashboard. */
export function WebModelConnectorDiagnostics({
  connector,
  disabled,
  onRefresh,
}: {
  connector: WebModelConnector;
  disabled: boolean;
  onRefresh: () => void;
}) {
  const { t } = useTranslation("settings");
  const events = [...(connector.events_log ?? []), ...(connector.security_events_log ?? [])].sort(
    (left, right) => right.at.localeCompare(left.at),
  );
  return (
    <SettingsCard
      title={t("webModelDiagnostics.title")}
      description={t("webModelDiagnostics.hint")}
      actions={
        <button
          type="button"
          className={secondaryButtonClass("sm")}
          disabled={disabled}
          onClick={onRefresh}
        >
          {t("webModelDiagnostics.refresh")}
        </button>
      }
    >
      <p
        data-web-model-last-rotation
        className="px-5 pt-3.5 text-xs text-[var(--color-text-secondary)]"
      >
        {connector.secret_rotated_at
          ? t("webModelDiagnostics.lastRotation", {
              time: formatMessageTimestamp(connector.secret_rotated_at),
              actor: connector.rotated_by || t("webModelDiagnostics.unknownActor"),
            })
          : t("webModelDiagnostics.lastRotationUnknown")}
      </p>
      {events.length ? (
        <ul className="divide-y divide-[var(--glass-border-subtle)]">
          {events.map((event, index) => {
            const rejected = event.reason === "invalid_secret";
            const rotated = event.reason === "secret_rotated";
            const failed = rejected || event.ok === false;
            const transport = event.transport
              ? t(`webModelDiagnostics.transports.${event.transport}`)
              : "-";
            return (
              <li
                key={`${event.at}-${index}`}
                data-web-model-call={
                  rejected ? "rejected" : rotated ? "rotated" : event.ok ? "ok" : "failed"
                }
                className="flex flex-wrap items-center justify-between gap-2 px-5 py-2.5 text-sm"
              >
                <span className="break-all font-mono text-xs">
                  {rejected
                    ? t("webModelDiagnostics.invalidSecret")
                    : rotated
                      ? t("webModelDiagnostics.rotated")
                      : event.tool_name || event.method || t("webModelDiagnostics.unknownCall")}
                </span>
                <span
                  data-status-tone={failed ? "error" : "ok"}
                  className={
                    failed
                      ? "break-words text-xs text-rose-700 dark:text-rose-300"
                      : "text-xs text-emerald-700 dark:text-emerald-300"
                  }
                >
                  {rejected
                    ? t("webModelDiagnostics.invalidSecretStatus", {
                        transport,
                        count: event.count ?? 1,
                      })
                    : rotated
                      ? t("webModelDiagnostics.rotatedStatus", {
                          generation: event.credential_generation ?? "-",
                        })
                      : event.ok
                        ? t("webModelDiagnostics.callOk")
                        : t("webModelDiagnostics.callFailed", { reason: event.reason || "-" })}
                </span>
                <span className="w-full text-xs text-[var(--color-text-muted)] sm:w-auto">
                  {formatMessageTimestamp(event.at)}
                </span>
                {rejected && (
                  <p className="w-full text-xs text-[var(--color-text-secondary)]">
                    {t("webModelDiagnostics.invalidSecretHint")}
                  </p>
                )}
                {rotated && (
                  <p className="w-full text-xs text-[var(--color-text-secondary)]">
                    {t("webModelDiagnostics.rotatedHint", {
                      actor: event.rotated_by || t("webModelDiagnostics.unknownActor"),
                      count: event.recorded_binding_count ?? 0,
                    })}
                  </p>
                )}
                {!event.ok && !rejected && !rotated && event.tool_name && (
                  <p className="w-full text-xs text-[var(--color-text-secondary)]">
                    {t(`webModelDiagnostics.reasons.${reasonHint(event.reason ?? null)}`)}
                  </p>
                )}
                {event.host_identity && (
                  <details className="w-full text-xs text-[var(--color-text-secondary)]">
                    <summary className="cursor-pointer">
                      {t("webModelDiagnostics.identity.title")}
                    </summary>
                    <p className="mt-2">{t("webModelDiagnostics.identity.hint")}</p>
                    <dl className="mt-2 grid gap-2">
                      {(["session", "subject", "organization"] as const).map((field) => (
                        <div key={field}>
                          <dt>{t(`webModelDiagnostics.identity.${field}`)}</dt>
                          <dd className="break-all font-mono">
                            {event.host_identity?.[field] ??
                              t("webModelDiagnostics.identity.absent")}
                          </dd>
                        </div>
                      ))}
                    </dl>
                  </details>
                )}
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="px-5 py-3.5 text-sm text-[var(--color-text-muted)]">
          {t("webModelDiagnostics.noCalls")}
        </p>
      )}
    </SettingsCard>
  );
}
