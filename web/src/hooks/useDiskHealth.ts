import { useEffect, useState } from "react";
import { apiJson } from "../services/api/base";

export type DiskHealth = {
  scope: "cccc_home";
  severity: "normal" | "warning" | "critical" | "unknown";
  available_bytes?: number;
  used_percent?: number;
};

export function useDiskHealth(): DiskHealth | null {
  const [disk, setDisk] = useState<DiskHealth | null>(null);
  useEffect(() => {
    let cancelled = false;
    let pending = false;
    let supported = false;
    const refresh = async () => {
      if (pending || cancelled) return;
      pending = true;
      try {
        const response = await apiJson<{ disk?: DiskHealth }>("/api/v1/health");
        if (cancelled) return;
        const current = response.ok ? response.result.disk : undefined;
        if (!current || current.scope !== "cccc_home") {
          setDisk(null);
          return;
        }
        supported = true;
        const valid = ["normal", "warning", "critical", "unknown"].includes(current.severity);
        const metrics =
          Number.isFinite(current.available_bytes) &&
          Number(current.available_bytes) >= 0 &&
          Number.isFinite(current.used_percent) &&
          Number(current.used_percent) >= 0 &&
          Number(current.used_percent) <= 100;
        setDisk(
          valid && (current.severity === "unknown" || metrics)
            ? current
            : { scope: "cccc_home", severity: "unknown" },
        );
      } catch {
        if (!cancelled && supported) setDisk({ scope: "cccc_home", severity: "unknown" });
      } finally {
        pending = false;
      }
    };
    void refresh();
    const timer = window.setInterval(() => {
      void refresh();
    }, 30_000);
    window.addEventListener("focus", refresh);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
      window.removeEventListener("focus", refresh);
    };
  }, []);
  return disk;
}
