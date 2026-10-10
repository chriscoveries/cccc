// DeliveryTab configures runtime delivery pacing and one-shot reminder windows.
import { useTranslation } from "react-i18next";

import { ClockIcon } from "../../Icons";
import { NumberInputRow } from "./automationUtils";
import {
  primaryButtonClass,
  settingsWorkspaceActionBarClass,
  settingsWorkspaceBodyClass,
  settingsWorkspaceHeaderClass,
  settingsWorkspaceShellClass,
  settingsWorkspaceFieldsClass,
} from "./types";

interface DeliveryTabProps {
  isDark: boolean;
  busy: boolean;
  mailWakeOnIdle: boolean;
  setMailWakeOnIdle: (v: boolean) => void;
  mailWakeMinAgeSeconds: number;
  setMailWakeMinAgeSeconds: (v: number) => void;
  taskWakeOnIdle: boolean;
  setTaskWakeOnIdle: (v: boolean) => void;
  taskWakeIntervalSeconds: number;
  setTaskWakeIntervalSeconds: (v: number) => void;
  mailNoticeAfterSeconds: number;
  setMailNoticeAfterSeconds: (v: number) => void;
  mailNoticeIdleAfterSeconds: number;
  setMailNoticeIdleAfterSeconds: (v: number) => void;
  replyNoticeAfterSeconds: number;
  setReplyNoticeAfterSeconds: (v: number) => void;
  onSave: () => void;
}

export function DeliveryTab(props: DeliveryTabProps) {
  const { isDark, busy, onSave } = props;
  const { t } = useTranslation("settings");

  return (
    <div className="animate-in fade-in slide-in-from-bottom-2 duration-300">
      <div className={settingsWorkspaceShellClass(isDark)}>
        <div className={settingsWorkspaceHeaderClass(isDark)}>
          <div className="min-w-0">
            <h3 className="text-sm font-semibold text-[var(--color-text-primary)]">
              {t("delivery.title")}
            </h3>
            <p className="mt-1 text-xs text-[var(--color-text-muted)]">
              {t("delivery.description")}
            </p>
          </div>
        </div>

        <div className={`${settingsWorkspaceBodyClass} grid grid-cols-1 gap-3 lg:grid-cols-2`}>
          <div className={`${settingsWorkspaceFieldsClass} lg:col-span-2`}>
            <label className="flex items-center gap-2 text-sm text-[var(--color-text-primary)]">
              <input
                type="checkbox"
                checked={props.mailWakeOnIdle}
                disabled={busy}
                onChange={(event) => props.setMailWakeOnIdle(event.target.checked)}
              />
              {t("delivery.mailWakeOnIdle")}
            </label>
            <p className="mt-1 text-xs text-[var(--color-text-muted)]">
              {t("delivery.mailWakeOnIdleHelp")}
            </p>
            {props.mailWakeOnIdle && (
              <NumberInputRow
                isDark={isDark}
                label={t("delivery.mailWakeMinAge")}
                value={props.mailWakeMinAgeSeconds}
                formatValue={false}
                onChange={(value) =>
                  props.setMailWakeMinAgeSeconds(
                    Number.isFinite(value) ? Math.max(0, Math.trunc(value)) : 0,
                  )
                }
              />
            )}
          </div>
          <div className={`${settingsWorkspaceFieldsClass} lg:col-span-2`}>
            <label className="flex items-center gap-2 text-sm text-[var(--color-text-primary)]">
              <input
                type="checkbox"
                checked={props.taskWakeOnIdle}
                disabled={busy}
                onChange={(event) => props.setTaskWakeOnIdle(event.target.checked)}
              />
              {t("delivery.taskWakeOnIdle")}
            </label>
            <p className="mt-1 text-xs text-[var(--color-text-muted)]">
              {t("delivery.taskWakeOnIdleHelp")}
            </p>
            {props.taskWakeOnIdle && (
              <NumberInputRow
                isDark={isDark}
                label={t("delivery.taskWakeInterval")}
                value={props.taskWakeIntervalSeconds}
                formatValue={false}
                onChange={(value) =>
                  props.setTaskWakeIntervalSeconds(
                    Number.isFinite(value) ? Math.max(60, Math.trunc(value)) : 60,
                  )
                }
              />
            )}
          </div>
          <div className={settingsWorkspaceFieldsClass}>
            <NumberInputRow
              isDark={isDark}
              label={t("delivery.mailNotice")}
              value={props.mailNoticeAfterSeconds}
              onChange={props.setMailNoticeAfterSeconds}
              helperText={t("delivery.mailNoticeHelp")}
            />
          </div>
          <div className={settingsWorkspaceFieldsClass}>
            <NumberInputRow
              isDark={isDark}
              label={t("delivery.mailIdleNotice")}
              value={props.mailNoticeIdleAfterSeconds}
              onChange={props.setMailNoticeIdleAfterSeconds}
              helperText={t("delivery.mailIdleNoticeHelp")}
            />
          </div>
          <div className={settingsWorkspaceFieldsClass}>
            <NumberInputRow
              isDark={isDark}
              label={t("delivery.replyNotice")}
              value={props.replyNoticeAfterSeconds}
              onChange={props.setReplyNoticeAfterSeconds}
              helperText={t("delivery.replyNoticeHelp")}
            />
          </div>
        </div>

        <div className={settingsWorkspaceActionBarClass(isDark)}>
          <button onClick={onSave} disabled={busy} className={primaryButtonClass(busy)}>
            {busy ? (
              t("common:saving")
            ) : (
              <span className="flex items-center gap-2">
                <ClockIcon className="w-4 h-4" /> {t("delivery.saveDelivery")}
              </span>
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
