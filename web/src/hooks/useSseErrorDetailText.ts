import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useUIStore } from "../stores/useUIStore";

/**
 * Composes the "why" for an interrupted stream: which call failed, its HTTP
 * status or reachability, and the live retry countdown — e.g.
 * "ledger stream HTTP 404 · retrying in 7s". Empty while connected.
 */
export function useSseErrorDetailText(): string {
  const { t } = useTranslation("layout");
  const sseError = useUIStore((s) => s.sseError);
  const nextRetryAt = sseError?.nextRetryAt ?? null;

  // `now` starts at 0 rather than Date.now(). Seeding it with the real clock
  // makes the very first render compute a countdown against a value captured
  // before the error arrived, and it only corrects one second later — the
  // count visibly starts one tick too high. Starting at 0 and syncing on the
  // first tick that follows a deadline being set renders the true remainder
  // immediately, and re-syncs whenever a new retry deadline arrives.
  const [now, setNow] = useState(0);

  useEffect(() => {
    if (!nextRetryAt) return;
    setNow(Date.now());
    const tick = () => setNow(Date.now());
    // Align to the deadline boundary so the displayed second flips when the
    // retry is actually due, rather than up to a second early.
    const timer = window.setInterval(tick, 250);
    return () => window.clearInterval(timer);
  }, [nextRetryAt]);

  if (!sseError) return "";
  const reason = `${sseError.endpoint}${
    sseError.status ? ` HTTP ${sseError.status}` : ` ${t("connectionUnreachable")}`
  }`;
  const countdown = nextRetryAt
    ? t("retryingIn", { seconds: Math.max(0, Math.ceil((nextRetryAt - now) / 1000)) })
    : "";
  return [reason, countdown].filter(Boolean).join(" · ");
}
