import type { PresentationBrowserSurfaceState, PresentationMessageRef } from "../types";

function asLocatorRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

export function canRestorePresentationRefInViewer(cardType: string): boolean {
  const normalized = String(cardType || "").trim();
  return normalized === "markdown" || normalized === "table";
}

export function getPresentationRefViewerScrollTop(
  ref: PresentationMessageRef | null | undefined,
): number | null {
  const locator = asLocatorRecord(ref?.locator);
  if (!locator) return null;
  const raw = locator.viewer_scroll_top;
  const value = typeof raw === "number" ? raw : Number(raw);
  if (!Number.isFinite(value) || value < 0) return null;
  return value;
}

export function shouldAutoOpenInteractivePresentation(
  allowLiveBrowser: boolean,
  session: Pick<PresentationBrowserSurfaceState, "active" | "state"> | null | undefined,
): boolean {
  if (!allowLiveBrowser || !session?.active) return false;
  const state = String(session.state || "").trim();
  return state === "starting" || state === "ready";
}
