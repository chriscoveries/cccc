function inspectChatgptModelPin() {
  // Only these two settled presets have retained, redacted DOM captures.
  // Never infer a family from the closed effort label or a numeric notch alone.
  const visible = (node) => {
    if (!node) return false;
    const rect = node.getBoundingClientRect();
    const style = getComputedStyle(node);
    return (
      rect.width > 0 && rect.height > 0 && style.display !== "none" && style.visibility !== "hidden"
    );
  };
  const text = (node) =>
    String(node?.innerText || node?.textContent || "")
      .replace(/\s+/g, " ")
      .trim();
  const buttons = Array.from(
    document.querySelectorAll('button[aria-label="Select ChatGPT model"]'),
  ).filter(visible);
  const menus = Array.from(document.querySelectorAll('[role="menu"]'))
    .filter(visible)
    .filter((menu) => menu.querySelector('[role="menuitemradio"]'));
  const result = {
    selection: null,
    family: "",
    power: null,
    button_text: "",
    reason: "picker_unreadable",
    busy: false,
  };
  result.busy = Array.from(document.querySelectorAll('[data-testid="stop-button"]')).some(visible);
  if (buttons.length !== 1 || menus.length !== 1) return result;
  const button = buttons[0],
    menu = menus[0];
  result.button_text = text(button);
  if (button.disabled || button.getAttribute("aria-disabled") === "true") return result;
  const radios = Array.from(menu.querySelectorAll('[role="menuitemradio"]')).filter(visible);
  if (
    radios.length !== 2 ||
    !radios.some((node) => text(node) === "GPT-6") ||
    !radios.some((node) => text(node) === "GPT-5.6 Sol")
  )
    return result;
  const selected = radios.filter((node) => node.getAttribute("aria-checked") === "true");
  const sliders = Array.from(menu.querySelectorAll('[role="slider"]')).filter(visible);
  const statuses = Array.from(menu.querySelectorAll('[role="status"]')).filter(visible);
  if (
    selected.length !== 1 ||
    sliders.length !== 1 ||
    statuses.length !== 1 ||
    selected[0].getAttribute("aria-disabled") === "true" ||
    sliders[0].getAttribute("aria-disabled") === "true"
  )
    return result;
  const slider = sliders[0];
  result.family = text(selected[0]);
  if (slider.getAttribute("aria-valuemin") !== "0" || slider.getAttribute("aria-valuemax") !== "4")
    return result;
  const power = slider.getAttribute("aria-valuenow");
  if (!/^[0-4]$/.test(power || "")) return result;
  result.power = Number(power);
  if (result.family === "GPT-6" && result.power === 4) {
    result.reason = "forbidden_model";
    return result;
  }
  if (result.busy) {
    result.reason = "provider_busy";
    return result;
  }
  if (
    result.family === "GPT-5.6 Sol" &&
    result.power === 4 &&
    text(statuses[0]) === "Pro, 5 of 5." &&
    result.button_text === "Thinking effort 5.6 Pro"
  ) {
    result.selection = { model: "gpt-5.6-pro", effort: null };
  } else if (
    result.family === "GPT-6" &&
    result.power === 3 &&
    text(statuses[0]) === "Extra High, 4 of 5." &&
    result.button_text === "Thinking effort Extra High"
  ) {
    result.selection = { model: "gpt-6", effort: "extra_high" };
  }
  result.reason = result.selection ? "" : "selection_unrecognized";
  return result;
}
