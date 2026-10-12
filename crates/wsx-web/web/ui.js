// Shared dialog and toast components. Theme hooks: .dialog, .toast.
import { add, h } from "./dom.js";

const region = () => document.getElementById("toasts");

// A theme with a dark scheme is dark unless this device chose light (docs/web-themes.md).
const SCHEME_KEY = "wsx-scheme";
export const hasDarkScheme = () => document.querySelector("meta[name=color-scheme]").content.includes("dark");
export function scheme() {
  try { return localStorage.getItem(SCHEME_KEY) === "light" ? "light" : "dark"; } catch { return "dark"; }
}
export function applyScheme() {
  if (!hasDarkScheme()) return;
  if (scheme() === "light") document.documentElement.dataset.scheme = "light";
  else delete document.documentElement.dataset.scheme;
  // The browser's own bars take the color of the page chrome.
  const chrome = getComputedStyle(document.documentElement).getPropertyValue("--color-chrome-bg").trim();
  if (chrome) document.querySelector("meta[name=theme-color]").content = chrome;
}
export function setScheme(value) {
  try { localStorage.setItem(SCHEME_KEY, value); } catch { /* a private window keeps the default */ }
  applyScheme();
}
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
