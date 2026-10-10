// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vite-plus/test";
import { LeftoverSection } from "./LeftoverSection";
import * as api from "../../../services/api/diagnostics";
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
vi.mock("../../../services/api/diagnostics", () => ({ fetchLeftoverProcesses: vi.fn() }));

describe("LeftoverSection", () => {
  let host: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;
  afterEach(async () => {
    await act(async () => root.unmount());
    host.remove();
    vi.clearAllMocks();
  });
  const render = async (props?: Partial<React.ComponentProps<typeof LeftoverSection>>) => {
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () =>
      root.render(
        <LeftoverSection isDark={false} groupId="g1" busy={false} onSave={vi.fn()} {...props} />,
      ),
    );
  };
  it("shows the count and rows after a scan", async () => {
    vi.mocked(api.fetchLeftoverProcesses).mockResolvedValue({
      ok: true,
      result: {
        count: 2,
        leftovers: [
          {
            pid: 4242,
            pgid: 4242,
            comm: "sleep",
            actor_id: "peer-a",
            group_id: "g1",
            class: "actor_stopped",
            started_secs: 1,
            age_secs: 90000,
          },
        ],
      },
    });
    await render();
    await act(async () => host.querySelector<HTMLButtonElement>("button")!.click());
    await vi.waitFor(() => expect(host.textContent).toContain("peer-a"));
    expect(host.textContent).toContain("actor_stopped");
    expect(host.textContent).toContain("(2)");
  });
  it("saves the reap toggle with a floored age", async () => {
    vi.mocked(api.fetchLeftoverProcesses).mockResolvedValue({ ok: true, result: { count: 0, leftovers: [] } });
    const save = vi.fn();
    await render({ onSave: save });
    const boxes = Array.from(host.querySelectorAll<HTMLInputElement>('input[type="checkbox"]'));
    await act(async () => boxes[boxes.length - 1]!.click());
    const buttons = Array.from(host.querySelectorAll<HTMLButtonElement>("button"));
    await act(async () => buttons[buttons.length - 1]!.click());
    expect(save).toHaveBeenCalledWith({
      reap_leftover_processes: true,
      reap_leftover_after_hours: 24,
    });
  });
  it("reports a scan failure without crashing", async () => {
    vi.mocked(api.fetchLeftoverProcesses).mockResolvedValue({
      ok: false,
      error: { code: "gone", message: "" },
    });
    await render();
    await act(async () => host.querySelector<HTMLButtonElement>("button")!.click());
    await vi.waitFor(() => expect(host.textContent).toContain("developer.leftoverFailed"));
  });
});
