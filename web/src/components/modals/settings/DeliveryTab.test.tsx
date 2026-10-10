// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vite-plus/test";
import { DeliveryTab } from "./DeliveryTab";
vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe("Mail idle opt-in controls", () => {
  let host: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;
  afterEach(async () => {
    await act(async () => root.unmount());
    host.remove();
  });
  it("hides the age when off and clamps enabled age edits to nonnegative integers", async () => {
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    const change = vi.fn();
    const toggle = vi.fn();
    const props = {
      isDark: false,
      busy: false,
      mailNoticeAfterSeconds: 1800,
      setMailNoticeAfterSeconds: vi.fn(),
      replyNoticeAfterSeconds: 900,
      setReplyNoticeAfterSeconds: vi.fn(),
      mailWakeOnIdle: false,
      setMailWakeOnIdle: toggle,
      mailWakeMinAgeSeconds: 60,
      setMailWakeMinAgeSeconds: change,
      onSave: vi.fn(),
    };
    await act(async () => root.render(<DeliveryTab {...props} />));
    expect(host.textContent).not.toContain("delivery.mailWakeMinAge");
    await act(async () => host.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click());
    expect(toggle).toHaveBeenCalledWith(true);
    await act(async () => root.render(<DeliveryTab {...props} mailWakeOnIdle />));
    const input = host.querySelector<HTMLInputElement>('input[type="number"]')!;
    expect(input.value).toBe("60");
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(
        input,
        "-2.5",
      );
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(change).toHaveBeenCalledWith(0);
  });
});
