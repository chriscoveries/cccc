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
    const taskChange = vi.fn();
    const taskToggle = vi.fn();
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
      taskWakeOnIdle: false,
      setTaskWakeOnIdle: taskToggle,
      taskWakeIntervalSeconds: 1800,
      setTaskWakeIntervalSeconds: taskChange,
      onSave: vi.fn(),
    };
    await act(async () => root.render(<DeliveryTab {...props} />));
    expect(host.textContent).not.toContain("delivery.mailWakeMinAge");
    expect(host.textContent).not.toContain("delivery.taskWakeInterval");
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
  it("hides the card interval when off and floors enabled edits at 60 seconds", async () => {
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    const taskChange = vi.fn();
    const taskToggle = vi.fn();
    const props = {
      isDark: false,
      busy: false,
      mailNoticeAfterSeconds: 1800,
      setMailNoticeAfterSeconds: vi.fn(),
      replyNoticeAfterSeconds: 900,
      setReplyNoticeAfterSeconds: vi.fn(),
      mailWakeOnIdle: false,
      setMailWakeOnIdle: vi.fn(),
      mailWakeMinAgeSeconds: 60,
      setMailWakeMinAgeSeconds: vi.fn(),
      taskWakeOnIdle: false,
      setTaskWakeOnIdle: taskToggle,
      taskWakeIntervalSeconds: 1800,
      setTaskWakeIntervalSeconds: taskChange,
      onSave: vi.fn(),
    };
    await act(async () => root.render(<DeliveryTab {...props} />));
    expect(host.textContent).not.toContain("delivery.taskWakeInterval");
    const boxes = Array.from(host.querySelectorAll<HTMLInputElement>('input[type="checkbox"]'));
    await act(async () => boxes[1]!.click());
    expect(taskToggle).toHaveBeenCalledWith(true);
    await act(async () => root.render(<DeliveryTab {...props} taskWakeOnIdle />));
    const inputs = Array.from(host.querySelectorAll<HTMLInputElement>('input[type="number"]'));
    const input = inputs[0]!;
    expect(input.value).toBe("1800");
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "10");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(taskChange).toHaveBeenCalledWith(60);
  });
});
