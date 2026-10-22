// The page's view library: Preact with htm templates, vendored as ES modules
// (scripts/vendor-preact.sh). No build step, and no inline script or style.
import { h, render } from "./preact.js";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "./preact-hooks.js";
import htm from "./htm.js";

export const html = htm.bind(h);
export { render, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState };

const ICONS = {
  back: "M12.5 4.5 7 10l5.5 5.5",
  chevron: "M7.5 4.5 13 10l-5.5 5.5",
  attach: "M14.5 9.5 9 15a3 3 0 0 1-4.2-4.2l6-6a2 2 0 0 1 2.8 2.8l-5.7 5.7a1 1 0 0 1-1.4-1.4L11 7.4",
  down: "M10 4v12M5 11l5 5 5-5",
  send: "M10 16V4M5 9l5-5 5 5",
  plus: "M10 4.5v11M4.5 10h11",
  pass: "M4.5 10.5 8 14l7.5-8",
  fail: "M5.5 5.5l9 9M14.5 5.5l-9 9",
  play: "M7 4.5v11l8.5-5.5z",
  open: "M11 4.5h4.5V9M15.5 4.5 9 11M8 5.5H5.5a1 1 0 0 0-1 1v8a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V12",
};
// A line icon. The control around it carries the accessible name.
export const Icon = ({ name }) => html`
  <svg class="icon" viewBox="0 0 20 20" width="20" height="20" fill="none" stroke="currentColor"
    stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <path d=${ICONS[name]} />
  </svg>`;

export function bytes(count) {
  if (count < 1024) return `${count} B`;
  if (count < 1024 * 1024) return `${(count / 1024).toFixed(1)} KB`;
  return `${(count / 1024 / 1024).toFixed(1)} MB`;
}

// Elapsed time in tabular form: 42s, 3m 05s, 1h 02m.
export function elapsed(ms) {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${String(seconds % 60).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

// How long ago, in one short word: now, 5m ago, 3h ago, 2d ago.
export function ago(ms) {
  const minutes = Math.floor(Math.max(0, ms) / 60000);
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m ago`;
  if (minutes < 48 * 60) return `${Math.floor(minutes / 60)}h ago`;
  return `${Math.floor(minutes / 1440)}d ago`;
}

// A message's time: the clock today, the date and the clock before.
export function clock(ms) {
  const date = new Date(ms);
  const time = date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return new Date().toDateString() === date.toDateString() ? time
    : `${date.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`;
}

// The first sentence of a text, on one line, at most about 200 characters: what a worker's
// summary says first ("did x. Then y" → "did x.").
export function firstSentence(text) {
  const line = (text ?? "").trim().split("\n")[0].trim();
  const end = /[.!?](\s|$)/.exec(line);
  const sentence = end ? line.slice(0, end.index + 1) : line;
  return sentence.length > 200 ? `${sentence.slice(0, 199).trimEnd()}…` : sentence;
}

// The present time, renewed every second while `active` is true.
export function useNow(active) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!active) return undefined;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [active]);
  return now;
}

// A conversation log that keeps its reader at the end only while the reader is at the end.
// `log` is the scroller's ref callback; `atEnd` is a ref the page may set (a sent message goes
// to the end); `away` shows the jump button. The log is also pinned when its own box changes
// size (the reservoir under it opens, the keyboard opens): that fires no scroll event.
export function useFollowEnd() {
  const node = useRef(null);
  const atEnd = useRef(true);
  const watcher = useRef(null);
  const [away, setAway] = useState(false);
  const height = useRef(0);
  const pin = () => { if (atEnd.current && node.current) node.current.scrollTop = node.current.scrollHeight; };
  const scrolled = () => {
    const element = node.current;
    // A scroll event may still arrive after the view closed.
    if (!element) return;
    // ^ A box that changed height (a panel opened under it) scrolls without the reader:
    // a log at its end stays there. The event can come before the resize observer.
    if (element.clientHeight !== height.current) {
      height.current = element.clientHeight;
      if (atEnd.current) {
        pin();
        return;
      }
    }
    atEnd.current = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
    setAway(!atEnd.current);
  };
  // The log's height is one screen of this device: a `Fold` inside it stops there.
  const screen = (element) => element.style.setProperty("--screen-height", `${element.clientHeight}px`);
  const log = useCallback((element) => {
    watcher.current?.disconnect();
    watcher.current = null;
    node.current = element;
    if (!element) return;
    height.current = element.clientHeight;
    screen(element);
    watcher.current = new ResizeObserver(() => {
      screen(element);
      if (!atEnd.current) return scrolled();
      height.current = element.clientHeight;
      pin();
    });
    watcher.current.observe(element);
  }, []);
  const toEnd = () => { atEnd.current = true; pin(); setAway(false); };
  useLayoutEffect(pin);
  return { log, atEnd, away, scrolled, toEnd };
}

// A message taller than one screen shows that screen and "Show all" (docs/web-client.md
// "Screens"). `kind`, a class of the body, may set a shorter fold in its CSS.
export function Fold({ id, kind, more = "Show all", children }) {
  const body = useRef();
  const [open, setOpen] = useState(false);
  const [long, setLong] = useState(false);
  useLayoutEffect(() => {
    const node = body.current;
    const measure = () => { if (!node.hasAttribute("data-open")) setLong(node.scrollHeight > node.clientHeight + 1); };
    measure();
    // ^ The screen height changes the fold, and a diagram drawn later changes the content.
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    for (const child of node.children) observer.observe(child);
    return () => observer.disconnect();
  });
  return html`
    <div class=${kind ? `fold ${kind}` : "fold"} id=${id} ref=${body} data-clamped=${!open && long ? "" : undefined} data-open=${open ? "" : undefined}>${children}</div>
    ${(long || open) && html`
      <button class="link-action fold-more" type="button" aria-expanded=${open} aria-controls=${id} onClick=${() => setOpen(!open)}>
        ${open ? "Show less" : more}
      </button>`}`;
}
