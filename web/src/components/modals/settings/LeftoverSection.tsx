import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Button } from "../../ui/button";
import { fetchLeftoverProcesses, type LeftoverProcess } from "../../../services/api/diagnostics";
import type { GroupSettings } from "../../../types";
import {
  inputClass,
  labelClass,
  settingsWorkspaceSectionClass,
} from "./types";

interface LeftoverSectionProps {
  isDark: boolean;
  groupId?: string;
  busy: boolean;
  onSave: (patch: Partial<GroupSettings>) => Promise<unknown>;
}

function ageText(seconds: number): string {
  if (seconds < 3600) {
    return `${Math.max(0, Math.floor(seconds / 60))}m`;
  }
  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}h`;
  }
  return `${Math.floor(seconds / 86400)}d`;
}

export function LeftoverSection({ isDark: _isDark, groupId, busy, onSave }: LeftoverSectionProps) {
  void _isDark;
  const { t } = useTranslation("settings");
  const [leftovers, setLeftovers] = useState<LeftoverProcess[]>([]);
  const [count, setCount] = useState<number | null>(null);
  const [err, setErr] = useState("");
  const [loading, setLoading] = useState(false);
  const [reapOnIdle, setReapOnIdle] = useState(false);
  const [reapHours, setReapHours] = useState(24);
  const [saving, setSaving] = useState(false);

  const refresh = async () => {
    if (!groupId || loading) return;
    setLoading(true);
    setErr("");
    try {
      const resp = await fetchLeftoverProcesses(groupId);
      if (resp.ok) {
        setLeftovers(resp.result.leftovers ?? []);
        setCount(resp.result.count ?? 0);
      } else {
        setLeftovers([]);
        setCount(null);
        setErr(resp.error?.message || t("developer.leftoverFailed"));
      }
    } catch {
      setLeftovers([]);
      setCount(null);
      setErr(t("developer.leftoverFailed"));
    } finally {
      setLoading(false);
    }
  };

  const save = async () => {
    setSaving(true);
    try {
      await onSave({
        reap_leftover_processes: reapOnIdle,
        reap_leftover_after_hours: Math.max(0, Math.trunc(reapHours || 0)),
      });
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className={settingsWorkspaceSectionClass}>
      <div className="flex items-start justify-between gap-3">
        <div>
          <div className="text-sm font-semibold text-[var(--color-text-primary)]">
            {t("developer.leftoverTitle")}
            {count !== null && count > 0 ? ` (${count})` : ""}
          </div>
          <div className="text-xs mt-0.5 text-[var(--color-text-muted)]">
            {t("developer.leftoverHint")}
          </div>
        </div>
        <Button type="button" disabled={busy || loading || !groupId} onClick={() => void refresh()}>
          {loading ? t("common:loading") : t("developer.leftoverRefresh")}
        </Button>
      </div>

      {err && <div className="mt-2 text-xs text-rose-600 dark:text-rose-400">{err}</div>}

      {count !== null && count === 0 && !err && (
        <div className="mt-2 text-xs text-[var(--color-text-muted)]">
          {t("developer.leftoverNone")}
        </div>
      )}

      {leftovers.length > 0 && (
        <div className="mt-2 overflow-x-auto">
          <table className="w-full text-xs">
            <thead>
              <tr className="text-left text-[var(--color-text-muted)]">
                <th className="pr-3 py-1">{t("developer.leftoverPid")}</th>
                <th className="pr-3 py-1">{t("developer.leftoverActor")}</th>
                <th className="pr-3 py-1">{t("developer.leftoverClass")}</th>
                <th className="pr-3 py-1">{t("developer.leftoverAge")}</th>
                <th className="py-1">{t("developer.leftoverComm")}</th>
              </tr>
            </thead>
            <tbody>
              {leftovers.map((item) => (
                <tr key={`${item.group_id}:${item.actor_id}:${item.pid}:${item.class}`}>
                  <td className="pr-3 py-1 font-mono">{item.pid}</td>
                  <td className="pr-3 py-1 font-mono">{item.actor_id}</td>
                  <td className="pr-3 py-1 font-mono">{item.class}</td>
                  <td className="pr-3 py-1">{ageText(item.age_secs)}</td>
                  <td className="py-1 font-mono">{item.comm || "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <label className="mt-3 flex items-center gap-2 text-sm text-[var(--color-text-primary)]">
        <input
          type="checkbox"
          checked={reapOnIdle}
          disabled={busy || saving}
          onChange={(event) => setReapOnIdle(event.target.checked)}
        />
        {t("developer.leftoverReap")}
      </label>
      <p className="mt-1 text-xs text-[var(--color-text-muted)]">
        {t("developer.leftoverReapHelp")}
      </p>
      {reapOnIdle && (
        <div className="mt-2 max-w-xs">
          <label className={labelClass()}>{t("developer.leftoverReapHours")}</label>
          <input
            type="number"
            value={reapHours}
            min={0}
            disabled={busy || saving}
            onChange={(e) =>
              setReapHours(Number.isFinite(Number(e.target.value)) ? Math.max(0, Math.trunc(Number(e.target.value))) : 0)
            }
            className={inputClass()}
          />
        </div>
      )}
      <div className="mt-2">
        <Button type="button" disabled={busy || saving} onClick={() => void save()}>
          {t("developer.leftoverSave")}
        </Button>
      </div>
    </div>
  );
}
