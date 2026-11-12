function submitChatgptModelPin(payload, inspectSubmission, messages, requestSubmit, inspectPin) {
  const blocked = {
    action: "",
    invoked: false,
    unsafe_state: true,
    error: "model_pin_unavailable",
  };
  const pin = inspectPin();
  if (
    !pin.selection ||
    pin.busy ||
    pin.selection.model !== payload.selection.model ||
    pin.selection.effort !== payload.selection.effort
  )
    return blocked;
  const state = inspectSubmission(payload, messages());
  if (state.url !== payload.url || state.echo_found || !state.composer_exact || state.running)
    return blocked;
  // Only toggle this menu. No radio, slider, keyboard Enter or correction path.
  const button = document.querySelector('button[aria-label="Select ChatGPT model"]');
  if (button?.getAttribute("aria-expanded") === "true") button.click();
  return requestSubmit();
}
