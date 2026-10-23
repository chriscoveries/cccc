import { expect, it } from "vite-plus/test";
import capturedContext from "../../../../crates/cccc-mcp/tests/fixtures/captured_context.json";
import { normalizeTask } from "./base";

it("preserves server-computed dispatch exclusions on a captured task", () => {
  const captured = capturedContext.tasks.find(
    (task) =>
      task.status === "planned" &&
      !task.assignee &&
      (!task.waiting_on || task.waiting_on === "none") &&
      !task.blocked_by?.length,
  );
  expect(captured).toBeDefined();
  // A read-only snapshot with no idle candidates. This exercises Web decoding,
  // not runtime authority; the task's workload fields come from the capture.
  const task = normalizeTask({
    ...captured,
    dispatch_eligible: false,
    dispatch_block_reason: "no_idle_actor",
  });
  expect(task?.dispatch_eligible).toBe(false);
  expect(task?.dispatch_block_reason).toBe("no_idle_actor");
});
