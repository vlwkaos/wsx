// Pairing: the host owner runs `wsx web pair --site <file>` and reads out the code.
import { api } from "./api.js";
import { fill, h } from "./dom.js";

export function pairView(main, onPaired) {
  const error = h("p", { class: "error", role: "alert" });
  const code = h("input", {
    type: "text", id: "pair-code", name: "code", autocomplete: "one-time-code",
    autocapitalize: "characters", spellcheck: false, required: true, maxLength: 16,
  });
  const name = h("input", { type: "text", id: "pair-name", name: "name", maxLength: 40, placeholder: "for example: my phone" });
  const submit = h("button", { class: "button-primary", type: "submit" }, "Pair this device");
  const form = h(
    "form",
    {
      class: "pair",
      onsubmit: async (event) => {
        event.preventDefault();
        submit.disabled = true;
        error.textContent = "";
        try {
          await api("api/pair", { method: "POST", json: { code: code.value, name: name.value } });
          onPaired();
        } catch (failure) {
          // The typed code stays in the field so a typo can be fixed.
          error.textContent = failure.message;
          submit.disabled = false;
          code.focus();
        }
      },
    },
    h("h2", {}, "Pair this device"),
    h("p", { class: "muted" }, "Ask the owner for a one-time code. It works once and expires after 10 minutes."),
    h("div", {}, h("label", { for: "pair-code" }, "Pairing code"), code),
    h("div", {}, h("label", { for: "pair-name" }, "Device name (optional)"), name),
    submit,
    error,
  );
  fill(main, form);
  code.focus();
}
