// Shared dialog and toast components. Theme hooks: .dialog, .toast.
import { add, h } from "./dom.js";

const region = () => document.getElementById("toasts");
const sticky = new Map();

// A named notice stays until it is cleared: `notice("link", "Connection lost")`,
// then `notice("link", null)`. An actionable failure must not vanish by itself.
export function notice(name, text) {
  sticky.get(name)?.remove();
  sticky.delete(name);
  if (!text) return;
  const node = h("p", { class: "toast", "data-kind": "error" }, text);
  sticky.set(name, node);
  add(region(), node);
}

// A result message that clears itself, for outcomes that need no action.
export function toast(text) {
  const node = h("p", { class: "toast" }, text);
  add(region(), node);
  setTimeout(() => node.remove(), 4000);
}

// Modal confirmation on the native <dialog>: focus is trapped while it is open,
// Escape cancels, and focus returns to the control that opened it.
export function confirmDialog({ title, text, confirm, danger = false }) {
  return new Promise((resolve) => {
    const opener = document.activeElement;
    const dialog = h(
      "dialog",
      { class: "dialog", "aria-labelledby": "dialog-title" },
      h("h2", { id: "dialog-title" }, title),
      h("p", {}, text),
      h(
        "div",
        { class: "row" },
        h("button", { type: "button", autofocus: true, onclick: () => dialog.close("cancel") }, "Cancel"),
        h("button", { type: "button", class: danger ? "button-danger" : "button-primary", onclick: () => dialog.close("confirm") }, confirm),
      ),
    );
    dialog.addEventListener("close", () => {
      dialog.remove();
      opener?.focus?.();
      resolve(dialog.returnValue === "confirm");
    });
    add(document.body, dialog);
    dialog.showModal();
  });
}
