// @vitest-environment happy-dom
import { readFileSync } from "node:fs";
import { afterEach, expect, it, vi } from "vite-plus/test";

const sourceRoot = new URL("../../../crates/cccc-web/", import.meta.url);
const read = (path: string) => readFileSync(new URL(path, sourceRoot), "utf8");
const inspectPin = new Function(`return (${read("src/browser_surface/chatgpt_model_pin.js")})`)();
const guardedSubmit = new Function(
  `return (${read("src/browser_surface/chatgpt_model_pin_submit.js")})`,
)();
const submissionSource = read("src/browser_surface/prompt_submission.rs");
function productionFunction(name: string) {
  const script = submissionSource.split(`const ${name}: &str = r#"`)[1]?.split('"#;')[0];
  if (!script) throw new Error(`Production script missing: ${name}`);
  return new Function(`return (${script})`)();
}
const inspectSubmission = productionFunction("INSPECT_SUBMISSION_SCRIPT");
const requestSubmit = productionFunction("REQUEST_SUBMIT_SCRIPT");

type CapturedNode = { tag: string; attrs: Record<string, string>; text: string };
type Capture = {
  pre: { buttonText: string; buttonAttrs: Record<string, string> };
  menu: { menuAttrs: Record<string, string>; nodes: CapturedNode[] };
};
function capturedPage(name: string) {
  const capture = JSON.parse(read(`tests/fixtures/model-pin/${name}.json`)) as Capture;
  document.body.replaceChildren();
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({
    width: 100,
    height: 20,
    x: 0,
    y: 0,
    top: 0,
    left: 0,
    right: 100,
    bottom: 20,
    toJSON: () => ({}),
  });
  const button = document.createElement("button");
  for (const [key, value] of Object.entries(capture.pre.buttonAttrs))
    button.setAttribute(key, value);
  button.setAttribute("aria-expanded", "true");
  button.textContent = capture.pre.buttonText;
  const menu = document.createElement("div");
  for (const [key, value] of Object.entries(capture.menu.menuAttrs)) menu.setAttribute(key, value);
  for (const node of capture.menu.nodes) {
    const element = document.createElement(node.tag);
    for (const [key, value] of Object.entries(node.attrs)) element.setAttribute(key, value);
    element.textContent = node.text;
    menu.append(element);
  }
  const form = document.createElement("form");
  const composer = document.createElement("textarea");
  composer.setAttribute("data-cccc-web-model-composer", "cccc-web-model-composer");
  // Opaque protocol control from the retained capture, not invented chat data.
  composer.value = menu.querySelector('[role="status"]')!.textContent!;
  form.append(composer);
  document.body.append(button, menu, form);
  const submitted = vi.fn();
  Object.defineProperty(form, "requestSubmit", { value: submitted, configurable: true });
  const selection =
    name === "pro-5.6"
      ? { model: "gpt-5.6-pro", effort: null }
      : { model: "gpt-6", effort: "extra_high" };
  const payload = {
    prompt: composer.value,
    needles: [composer.value],
    url: location.href,
    selection,
  };
  const send = () => guardedSubmit(payload, inspectSubmission, () => [], requestSubmit, inspectPin);
  return { button, menu, composer, submitted, payload, send };
}
afterEach(() => {
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

for (const name of ["pro-5.6", "gpt6-xhigh"]) {
  it(`recognizes the real ${name} preset without trusting the data effort attribute`, () => {
    const page = capturedPage(name);
    expect(inspectPin().selection).toEqual(page.payload.selection);
    expect(page.send().invoked).toBe(true);
    expect(page.submitted).toHaveBeenCalledTimes(1);
  });
}

for (const mutation of [
  "forbidden",
  "wrong-family",
  "unknown",
  "disabled",
  "busy",
  "missing",
  "draft",
  "url",
] as const) {
  it(`refuses final activation after ${mutation} fault without changing any radio`, () => {
    const page = capturedPage("gpt6-xhigh");
    expect(inspectPin().selection).toEqual(page.payload.selection); // Initial admission was valid.
    const radios = [...page.menu.querySelectorAll<HTMLElement>('[role="menuitemradio"]')];
    const clicked = vi.fn();
    radios.forEach((radio) => radio.addEventListener("click", clicked));
    if (mutation === "forbidden")
      page.menu.querySelector('[role="slider"]')!.setAttribute("aria-valuenow", "4");
    if (mutation === "wrong-family")
      radios.forEach((radio) =>
        radio.setAttribute("aria-checked", String(radio.textContent !== "GPT-6")),
      );
    if (mutation === "unknown") page.menu.querySelector('[role="status"]')!.textContent = "";
    if (mutation === "disabled")
      radios
        .find((radio) => radio.getAttribute("aria-checked") === "true")!
        .setAttribute("aria-disabled", "true");
    if (mutation === "busy") {
      const stop = document.createElement("button");
      stop.dataset.testid = "stop-button";
      page.menu.append(stop);
    }
    if (mutation === "missing") page.menu.remove();
    if (mutation === "draft") page.composer.value = "";
    if (mutation === "url") page.payload.url += "/changed-control";
    expect(page.send().invoked).toBe(false);
    expect(page.submitted).not.toHaveBeenCalled();
    expect(clicked).not.toHaveBeenCalled();
  });
}
