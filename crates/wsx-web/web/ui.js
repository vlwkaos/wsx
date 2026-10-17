// Notices, the confirmation dialog, and the device's light or dark choice.
import { html, Icon, useEffect, useState } from "./lib.js";
import { backTarget, can, goBack, parseRoute, reloadForBuild, state, useStore } from "./store.js";

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
  // The browser's own bars take the page background.
  const background = getComputedStyle(document.documentElement).getPropertyValue("--color-bg").trim();
  if (background) document.querySelector("meta[name=theme-color]").content = background;
}
export function setScheme(value) {
  try { localStorage.setItem(SCHEME_KEY, value); } catch { /* a private window keeps the default */ }
  applyScheme();
}

// A result message that clears itself, for outcomes that need no action.
let toasts = [];
const watchers = new Set();
export function toast(text) {
  const entry = { text };
  toasts = [...toasts, entry];
  for (const watcher of watchers) watcher();
  setTimeout(() => {
    toasts = toasts.filter((other) => other !== entry);
    for (const watcher of watchers) watcher();
  }, 4000);
}

// One line per condition, above the screen. A lost connection stays until it is back.
export function Notices() {
  const store = useStore();
  const [, renew] = useState(0);
  useEffect(() => {
    const watcher = () => renew((count) => count + 1);
    watchers.add(watcher);
    return () => watchers.delete(watcher);
  }, []);
  return html`
    <div class="notices" role="status" aria-live="polite">
      ${!store.online && html`<p class="notice" data-tone="attention">Connection lost. Reconnecting…</p>`}
      ${store.newBuild && html`<button class="build-pill" type="button" onClick=${reloadForBuild}>New version · Reload</button>`}
      ${toasts.slice(-2).map((entry) => html`<p class="notice">${entry.text}</p>`)}
    </div>`;
}

// Modal confirmation on the native <dialog>: focus is trapped while it is open,
// Escape cancels, and focus returns to the control that opened it.
export function confirmDialog({ title, text, confirm, danger = false }) {
  return new Promise((done) => {
    const opener = document.activeElement;
    const dialog = document.createElement("dialog");
    dialog.className = "sheet";
    dialog.setAttribute("aria-labelledby", "dialog-title");
    const element = (tag, props, ...children) => {
      const node = Object.assign(document.createElement(tag), props);
      node.append(...children);
      return node;
    };
    const cancel = element("button", { type: "button", className: "button", autofocus: true, textContent: "Cancel" });
    const accept = element("button", { type: "button", className: danger ? "button button-danger" : "button button-primary", textContent: confirm });
    cancel.addEventListener("click", () => dialog.close("cancel"));
    accept.addEventListener("click", () => dialog.close("confirm"));
    dialog.append(
      element("h2", { id: "dialog-title", className: "sheet-title", textContent: title }),
      element("p", { className: "muted", textContent: text }),
      element("div", { className: "actions sheet-actions" }, cancel, accept),
    );
    dialog.addEventListener("close", () => {
      dialog.remove();
      opener?.focus?.();
      done(dialog.returnValue === "confirm");
    });
    document.body.append(dialog);
    dialog.showModal();
  });
}

// The back control of a screen's top row: to the screen it was opened from, else `fallback`.
export function Back({ fallback }) {
  const target = backTarget(fallback);
  const route = parseRoute(target);
  const project = state.projects.find((candidate) => candidate.id === route.id)?.name ?? "project";
  const names = {
    home: "home", inbox: can("task_view") ? "all sessions" : "inbox", projects: "projects", settings: "settings", project, files: `files of ${project}`,
    sessions: `sessions of ${project}`, task: `task #${route.task}`,
    session: state.sessions.find((candidate) => candidate.id === route.id)?.label ?? "session",
  };
  return html`
    <a class="icon-button" href=${target} aria-label=${`Back to ${names[route.screen]}`} onClick=${goBack}>
      <${Icon} name="back" />
    </a>`;
}
