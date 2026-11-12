// Notices, the confirmation dialog, and the device's light or dark choice.
import { api } from "./api.js";
import { html, Icon, keep, kept, Loading, useCoalescedLoad, useEffect, useExit, useRef, useState } from "./lib.js";
import { backTarget, can, goBack, parseRoute, reloadForBuild, state, update, useEventLoad, useStore } from "./store.js";

// A theme with a dark scheme is dark unless this device chose light (docs/web-themes.md).
const SCHEME_KEY = "wsx-scheme";
export const hasDarkScheme = () => document.querySelector("meta[name=color-scheme]").content.includes("dark");
export function scheme() {
  return kept(SCHEME_KEY) === "light" ? "light" : "dark";
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
  keep(SCHEME_KEY, value);
  applyScheme();
}

// ^ Where the header bar sits on this device. app.css moves every `.bar` by `html[data-bar]`;
// the project chat reads `state.bar` to put its bar above the composer.
const BAR_KEY = "wsx-bar";
export function applyBar() {
  const side = kept(BAR_KEY) === "bottom" ? "bottom" : "top";
  if (side === "bottom") document.documentElement.dataset.bar = "bottom";
  else delete document.documentElement.dataset.bar;
  update({ bar: side });
}
export function setBar(side) {
  keep(BAR_KEY, side);
  applyBar();
}

// ^ The bottom-bar tabs this device hides, as a list of names (settings.js UI). Settings
// cannot be hidden: it is left out here and in `shownTabs`.
const TABS_KEY = "wsx-tabs-hidden";
export function applyTabs() {
  let hidden = [];
  try {
    const stored = JSON.parse(kept(TABS_KEY));
    if (Array.isArray(stored)) hidden = stored.filter((name) => typeof name === "string" && name !== "settings");
  } catch { /* the default */ }
  update({ hiddenTabs: hidden });
}
export function setTabHidden(name, hidden) {
  const rest = state.hiddenTabs.filter((other) => other !== name);
  keep(TABS_KEY, JSON.stringify(hidden && name !== "settings" ? [...rest, name] : rest));
  applyTabs();
}

// One notice over the screen at a time, never in the layout: an outcome clears itself after a
// few seconds; a failure the owner must act on stays until dismissed, and a later outcome does
// not hide it (docs/web-client.md "Notices").
const TOAST_MS = 4000;
let toasts = [];
let lastId = 0;
const watchers = new Set();
const changed = () => { for (const watcher of watchers) watcher(); };
// ^ A row that slides left to show Remove (.row-slide) is filled only while it is slid open.
export const slid = (event) => event.currentTarget.toggleAttribute("data-open", event.currentTarget.scrollLeft > 0);
export function toast(text, { failure = false } = {}) {
  const entry = { text, failure, id: ++lastId };
  toasts = [...toasts.filter((other) => other.failure), entry];
  changed();
  if (!failure) {
    setTimeout(() => {
      toasts = toasts.filter((other) => other !== entry);
      changed();
    }, TOAST_MS);
  }
}
const dismiss = (entry) => {
  toasts = toasts.filter((other) => other !== entry);
  changed();
};

const OFFLINE = { text: "Connection lost. Reconnecting…" };
// A lost connection stays until it is back; then the newest failure, else the newest outcome.
export function Notices() {
  const store = useStore();
  const [, renew] = useState(0);
  useEffect(() => {
    const watcher = () => renew((count) => count + 1);
    watchers.add(watcher);
    return () => watchers.delete(watcher);
  }, []);
  const shown = toasts.findLast((entry) => entry.failure) ?? toasts.at(-1);
  // ^ One notice at a time; it slides out when it goes (docs/web-client.md "Motion").
  const [notice, leaving] = useExit(!store.online ? OFFLINE : shown);
  return html`
    <div class="notices" role="status" aria-live="polite">
      ${notice === OFFLINE ? html`<p class="notice" key="offline" data-tone="attention" data-offline="" data-leaving=${leaving ? "" : null}>${OFFLINE.text}</p>`
        : notice && html`
          <p class="notice" key=${notice.id} data-tone=${notice.failure ? "danger" : null} data-leaving=${leaving ? "" : null} role=${notice.failure ? "alert" : null}>
            <span class="notice-text">${notice.text}</span>
            ${notice.failure && html`<button class="icon-button" type="button" title="Dismiss" aria-label="Dismiss" onClick=${() => dismiss(notice)}><${Icon} name="fail" /></button>`}
            ${!notice.failure && html`<span class="notice-timer" aria-hidden="true"></span>`}
          </p>`}
    </div>
    ${store.newBuild && html`<button class="build-pill" type="button" onClick=${reloadForBuild}>New version · Reload</button>`}`;
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
    sessions: `sessions of ${project}`, task: `task #${route.task}`, chat: `chat ${route.chat}`,
    session: state.sessions.find((candidate) => candidate.id === route.id)?.label ?? "session",
  };
  return html`
    <a class="icon-button" href=${target} aria-label=${`Back to ${names[route.screen]}`} onClick=${(event) => { event.preventDefault(); goBack(fallback); }}>
      <${Icon} name="back" />
    </a>`;
}

// A top row's ⋯ menu. A tap outside or Escape closes it, and so does choosing an item.
export function MoreMenu({ children }) {
  const [open, setOpen] = useState(false);
  const menu = useRef();
  useEffect(() => {
    if (!open) return undefined;
    const outside = (event) => { if (!menu.current?.contains(event.target)) setOpen(false); };
    const escape = (event) => { if (event.key === "Escape") { setOpen(false); menu.current?.querySelector("button")?.focus(); } };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => { document.removeEventListener("pointerdown", outside); document.removeEventListener("keydown", escape); };
  }, [open]);
  return html`
    <div class="more" ref=${menu}>
      <button class="icon-button more-button" type="button" aria-label="More" aria-haspopup="menu" aria-expanded=${open}
        onClick=${() => setOpen(!open)}><span aria-hidden="true">⋯</span></button>
      ${open && html`<div class="more-menu" role="menu" onClick=${() => setOpen(false)}>${children}</div>`}
    </div>`;
}

// The top bar of a sheet page: Back to the chat, the title block (`children`) and the tools
// after it. The bar is also the sheet's grip: a drag on it moves the sheet (tasks.js `Sheet`).
export const SheetBar = ({ grip, onClose, tools, children }) => html`
  <header class="bar" onPointerDown=${grip?.down} onPointerMove=${grip?.move} onPointerUp=${grip?.up} onPointerCancel=${grip?.up}>
    <button class="icon-button" type="button" aria-label="Back to the chat" onClick=${onClose}><${Icon} name="back" /></button>
    <div class="bar-title">${children}</div>
    ${tools}
  </header>`;

// ^ A sheet page's one read of `path`: [value, failure, read]. It reads on open and when
// `again` changes, and on each stream event in `on` that `wanted` accepts (`useEventLoad`).
export function useSheetRead(path, { on = [], wanted = () => true, again = [] } = {}) {
  const [value, setValue] = useState(null);
  const [failure, setFailure] = useState("");
  const read = useCoalescedLoad(async (current) => {
    const next = await api(path);
    if (!current()) return;
    setValue(next);
    setFailure("");
  }, (error) => setFailure(error.message), [path]);
  useEventLoad(on, read, wanted, [path, ...again]);
  return [value, failure, read];
}

// ^ A sheet page: the bar (`bar` is its title block, `tools` follow it), then the scrolling body
// (`className`, `scroll` the ref). The sheet opens once the page is `loaded` or has a `failure`
// (tasks.js `Sheet`; `grip` is absent when the page is a screen). A failure or the wait takes the
// body, under `head`; `children` show once loaded.
export function SheetPage({ grip, onClose, tools, bar, className = "", scroll, head, loaded = true, failure = "", children }) {
  useEffect(() => { if (loaded || failure) grip?.ready(); }, [loaded, Boolean(failure)]);
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose} tools=${tools}>${bar}</${SheetBar}>
      <div class=${`scroll ${className}`.trim()} ref=${scroll}>
        ${head}
        ${failure ? html`<p class="error empty" role="alert">${failure}</p>` : !loaded ? html`<${Loading} />` : children}
      </div>
    </div>`;
}
