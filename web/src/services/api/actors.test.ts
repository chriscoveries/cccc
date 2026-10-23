import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { addActor, updateActor } from "./actors";

const fetchMock = vi.fn();

beforeEach(() => {
  vi.stubGlobal("window", { location: { search: "" } });
  vi.stubGlobal("fetch", fetchMock);
  fetchMock.mockReset();
  fetchMock.mockResolvedValue(
    new Response(JSON.stringify({ ok: true, result: {} }), { status: 200 }),
  );
});
afterEach(() => vi.unstubAllGlobals());

const body = () => JSON.parse(String(fetchMock.mock.calls[0][1]?.body || "{}"));

describe("Actor model_label requests", () => {
  it("sets the operator declaration on add", async () => {
    await addActor("g1", "peer", "peer", "custom", "", undefined, {
      ...{ model_label: "Declared model" },
    });
    expect(body().model_label).toBe("Declared model");
  });

  it("keeps the declaration absent when not supplied on add", async () => {
    await addActor("g1", "peer", "peer", "custom", "");
    expect(body()).not.toHaveProperty("model_label");
  });

  it.each(["Updated model", ""])(
    "updates model_label to %j, including an explicit clear",
    async (model_label) => {
      await updateActor("g1", "peer", undefined, undefined, undefined, { ...{ model_label } });
      expect(body()).toEqual({ by: "user", model_label });
    },
  );

  it("does not alter the declaration when absent from an update", async () => {
    await updateActor("g1", "peer", undefined, undefined, "Updated title");
    expect(body()).toEqual({ by: "user", title: "Updated title" });
  });
});
