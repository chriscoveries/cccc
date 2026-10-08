// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { beforeEach, expect, it, vi } from "vite-plus/test";
const { translate } = vi.hoisted(() => ({ translate: (key: string) => key }));
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: translate }) }));
vi.mock("../../services/api/actors", () => ({
  inspectActorClaudeLaunch: vi.fn(),
  resetActorClaudeLaunch: vi.fn(),
}));
import { inspectActorClaudeLaunch, resetActorClaudeLaunch } from "../../services/api/actors";
import { ClaudeLaunchRecoveryDialog } from "./ClaudeLaunchRecoveryDialog";
const inspection = {
  group_id: "g_one",
  actor_id: "worker",
  actor_generation: "generation",
  actor_created_at: "created",
  config_dir: "/original/claude",
  attempt_id: "attempt",
  session_id: "",
  resettable: true,
  jobs: [{ short: "abcdef12", sessionId: "survivor" }],
  inspection_error: null,
};
beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  document.body.innerHTML = "";
  vi.resetAllMocks();
  vi.mocked(inspectActorClaudeLaunch).mockResolvedValue({ ok: true, result: inspection });
});
it("shows original jobs and requires acknowledgment before resetting the exact scope", async () => {
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const onReset = vi.fn();
  const onOpenChange = vi.fn();
  vi.mocked(resetActorClaudeLaunch).mockResolvedValue({ ok: true, result: { cleared: true } });
  await act(async () => {
    root.render(
      <ClaudeLaunchRecoveryDialog
        open
        onOpenChange={onOpenChange}
        groupId="g_one"
        actorId="worker"
        onReset={onReset}
      />,
    );
  });
  expect(document.body.textContent).toContain("/original/claude");
  expect(document.body.textContent).toContain("abcdef12");
  const button = Array.from(document.querySelectorAll("button")).find(
    (b) => b.textContent === "clearClaudeLaunchFence",
  );
  expect(button?.disabled).toBe(true);
  expect(resetActorClaudeLaunch).not.toHaveBeenCalled();
  const checkbox = document.querySelector<HTMLInputElement>('input[type="checkbox"]');
  await act(async () => {
    checkbox?.click();
  });
  expect(button?.disabled).toBe(false);
  await act(async () => {
    button?.click();
  });
  expect(resetActorClaudeLaunch).toHaveBeenCalledWith(inspection);
  expect(onReset).toHaveBeenCalledOnce();
  expect(onOpenChange).toHaveBeenCalledWith(false);
  await act(async () => root.unmount());
  host.remove();
});
it("keeps the dialog open when the daemon refuses a stale generation", async () => {
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  const onReset = vi.fn();
  const onOpenChange = vi.fn();
  vi.mocked(resetActorClaudeLaunch).mockResolvedValue({
    ok: false,
    error: { code: "stale_claude_launch", message: "generation changed" },
  });
  await act(async () => {
    root.render(
      <ClaudeLaunchRecoveryDialog
        open
        onOpenChange={onOpenChange}
        groupId="g_one"
        actorId="worker"
        onReset={onReset}
      />,
    );
  });
  await act(async () => {
    document.querySelector<HTMLInputElement>('input[type="checkbox"]')?.click();
  });
  await act(async () => {
    Array.from(document.querySelectorAll("button"))
      .find((b) => b.textContent === "clearClaudeLaunchFence")
      ?.click();
  });
  expect(document.body.textContent).toContain("generation changed");
  expect(onReset).not.toHaveBeenCalled();
  expect(onOpenChange).not.toHaveBeenCalled();
  await act(async () => root.unmount());
  host.remove();
});
