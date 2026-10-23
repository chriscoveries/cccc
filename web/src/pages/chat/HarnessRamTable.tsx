// Harness RAM over the last 72 hours: what each (harness, mode) cost.
//
// Display only. The panel is collapsed until an operator asks for it, and it
// fetches once on open, so the dock stays as cheap as it was.
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

import { fetchMemoryReport } from "../../services/api/actors";
import type { HarnessRamKey, HarnessRamReport } from "../../types";
import { classNames } from "../../utils/classNames";
import { SPARK_HEIGHT, SPARK_WIDTH, formatMemoryMib, sparklinePoints } from "./harnessRamFormat";

function HarnessRamRow({ row }: { row: HarnessRamKey }) {
  const points = useMemo(() => sparklinePoints(row.series), [row.series]);
  const label = `${row.harness} ${row.mode}: peak ${formatMemoryMib(
    row.peak_rss_mib,
  )}, average ${formatMemoryMib(row.avg_rss_mib)} over ${row.hours_observed}h`;
  return (
    <tr title={label}>
      <td className="pr-2 py-0.5 whitespace-nowrap">{row.harness}</td>
      <td className="pr-2 py-0.5 whitespace-nowrap opacity-70">{row.mode}</td>
      <td className="pr-2 py-0.5 text-right tabular-nums">{formatMemoryMib(row.peak_rss_mib)}</td>
      <td className="pr-2 py-0.5 text-right tabular-nums">{formatMemoryMib(row.avg_rss_mib)}</td>
      <td className="pr-2 py-0.5 text-right tabular-nums opacity-70">{row.samples}</td>
      <td className="py-0.5">
        <svg
          width={SPARK_WIDTH}
          height={SPARK_HEIGHT}
          viewBox={`0 0 ${SPARK_WIDTH} ${SPARK_HEIGHT}`}
          aria-hidden="true"
          className="overflow-visible"
        >
          <polyline
            points={points}
            fill="none"
            stroke="currentColor"
            strokeWidth="1.2"
            strokeLinejoin="round"
            strokeLinecap="round"
            opacity="0.75"
          />
        </svg>
      </td>
    </tr>
  );
}

export function HarnessRamTable({ groupId, isDark }: { groupId: string; isDark: boolean }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [report, setReport] = useState<HarnessRamReport | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [attempt, setAttempt] = useState(0);

  // One request per open, and one more per retry. `loading` is deliberately not
  // a dependency: setting it here re-renders, and a dependency on it would tear
  // this effect down and cancel the request it had just sent, leaving the panel
  // on "Loading" for as long as the fetch takes. A request that is still in
  // flight when the panel closes is dropped, not applied.
  useEffect(() => {
    if (!open) return undefined;
    let cancelled = false;
    setLoading(true);
    void fetchMemoryReport(groupId).then((response) => {
      if (cancelled) return;
      setLoading(false);
      if (response.ok) {
        setReport(response.result);
        setError("");
        return;
      }
      setError(response.error.message || "unavailable");
    });
    return () => {
      cancelled = true;
    };
  }, [open, groupId, attempt]);

  const keys = report?.keys ?? [];
  return (
    <div className="absolute bottom-full right-0 mb-2 flex flex-col items-end">
      {open ? (
        <div
          className={classNames(
            "pointer-events-auto mb-1 w-[420px] max-w-[calc(100vw-3rem)] rounded-xl border px-3 py-2 text-[11px] shadow-lg backdrop-blur",
            isDark
              ? "border-white/10 bg-slate-950/92 text-slate-100"
              : "border-black/10 bg-white/95 text-[rgb(35,36,37)]",
          )}
        >
          <div className="mb-1 flex items-center justify-between">
            <span className="font-medium">
              {t("harnessRam72h", { defaultValue: "Harness RAM, 72 h" })}
            </span>
            <button
              type="button"
              onClick={() => setOpen(false)}
              className="rounded px-1 opacity-60 hover:opacity-100 focus-visible:outline-none focus-visible:ring-2"
              aria-label={t("close", { defaultValue: "Close" })}
            >
              ×
            </button>
          </div>
          {loading ? (
            <p className="opacity-60">{t("loading", { defaultValue: "Loading…" })}</p>
          ) : null}
          {error ? (
            <p className="text-red-400">
              {error}{" "}
              <button
                type="button"
                onClick={() => setAttempt((count) => count + 1)}
                className="underline underline-offset-2 hover:no-underline focus-visible:outline-none focus-visible:ring-2"
              >
                {t("retry", { defaultValue: "Retry" })}
              </button>
            </p>
          ) : null}
          {!loading && !error && keys.length === 0 ? (
            <p className="opacity-60">
              {t("harnessRamEmpty", { defaultValue: "No readings in the last 72 hours yet." })}
            </p>
          ) : null}
          {keys.length > 0 ? (
            <table className="w-full border-collapse">
              <caption className="sr-only">
                {t("harnessRam72h", { defaultValue: "Harness RAM, 72 h" })}
              </caption>
              <thead>
                <tr className="text-left opacity-55">
                  <th className="pr-2 font-normal">{t("harness", { defaultValue: "harness" })}</th>
                  <th className="pr-2 font-normal">{t("mode", { defaultValue: "mode" })}</th>
                  <th className="pr-2 text-right font-normal">
                    {t("peak", { defaultValue: "peak" })}
                  </th>
                  <th className="pr-2 text-right font-normal">
                    {t("avg", { defaultValue: "avg" })}
                  </th>
                  <th className="pr-2 text-right font-normal">
                    {t("samples", { defaultValue: "n" })}
                  </th>
                  <th className="font-normal">{t("trend", { defaultValue: "trend" })}</th>
                </tr>
              </thead>
              <tbody>
                {keys.map((row) => (
                  <HarnessRamRow key={`${row.harness}/${row.mode}`} row={row} />
                ))}
              </tbody>
            </table>
          ) : null}
        </div>
      ) : null}
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        aria-label={t("harnessRam72h", { defaultValue: "Harness RAM, 72 h" })}
        className={classNames(
          "pointer-events-auto rounded-full border px-2 py-0.5 text-[10px] uppercase tracking-wide opacity-70 transition-opacity hover:opacity-100 focus-visible:outline-none focus-visible:ring-2",
          isDark
            ? "border-white/12 bg-slate-950/80 text-slate-200"
            : "border-black/10 bg-white/90 text-[rgb(35,36,37)]",
        )}
      >
        {t("harnessRam", { defaultValue: "RAM" })}
      </button>
    </div>
  );
}
