import { useTranslation } from "react-i18next";
import type { WebModelBrowserSession, WebModelPairing } from "../../services/api";
import { webModelConnectionState } from "./webModelConnectionState";

export function WebModelManagedConnection() {
  const { t } = useTranslation("settings");
  return (
    <section className="min-w-0 space-y-2 text-sm" aria-label={t("webModelActor.title")}>
      <h3 className="font-semibold">{t("webModelActor.title")}</h3>
      <p role="status" className="text-[var(--color-text-secondary)]">
        {t("webModelActor.adminManaged")}
      </p>
    </section>
  );
}

export function WebModelConnectionStatus({
  pairing,
  session,
}: {
  pairing?: WebModelPairing;
  session?: WebModelBrowserSession | null;
}) {
  const { t, i18n } = useTranslation("settings");
  const { displayState, failed, bound, connecting } = webModelConnectionState(pairing, session);
  const pin = session?.health_snapshot?.model_pin;
  const selectionLabel = (value: { model: string; effort?: string | null } | null | undefined) =>
    value ? `${value.model}${value.effort ? ` / ${value.effort}` : ""}` : "Unknown";
  return (
    <div
      className="min-w-0 space-y-1 text-sm"
      role="status"
      title={
        session?.checked_at
          ? t("webModelActor.checkedAt", {
              time: new Date(session.checked_at).toLocaleString(i18n.language),
            })
          : undefined
      }
    >
      <span
        className={failed ? "text-amber-700 dark:text-amber-300" : undefined}
        data-status-tone={failed ? "warning" : undefined}
      >
        {t(`webModelActor.states.${displayState}`, {
          defaultValue: t("webModelActor.states.loading"),
        })}
      </span>
      {failed && pairing?.error_code && (
        <p
          role="alert"
          className="break-words text-amber-700 dark:text-amber-300"
          data-status-tone="warning"
        >
          {t(`webModelActor.errors.${pairing.error_code}`, {
            defaultValue: t("webModelActor.errors.pairing_failed"),
          })}
        </p>
      )}
      {bound && (failed || connecting) && (
        <p className="text-[var(--color-text-secondary)]">{t("webModelActor.bindingRetained")}</p>
      )}
      {pin && (
        <p
          data-model-pin-state={pin.state}
          className={
            pin.state === "held"
              ? "text-amber-700 dark:text-amber-300"
              : "text-[var(--color-text-secondary)]"
          }
        >
          Pinned: {pin.requested ? selectionLabel(pin.requested) : "Observe only"}; observed:{" "}
          {selectionLabel(pin.observed)}.
          {pin.state === "held" ? " Delivery held. Set the picker by hand, then resume." : ""}
          {pin.effective?.source === "fallback"
            ? ` Explicit fallback: ${selectionLabel(pin.effective.selection)}.`
            : ""}
        </p>
      )}
    </div>
  );
}
