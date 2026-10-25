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
  copy: "M7.5 7.5h8v8h-8zM12.5 4.5h-8v8",
  retry: "M15.5 10a5.5 5.5 0 1 1-1.6-3.9M15.5 4v3.5H12",
  reply: "M8.5 5.5 4 10l4.5 4.5M4.5 10H12a4 4 0 0 1 4 4v1",
  land: "M10 4v9M6 9.5l4 4 4-4M4.5 16h11",
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

// A map kept per device. A private window, cleared site data or a broken value reads as
// empty, and a write that fails keeps the page working without it.
export function readMap(key) {
  try { return JSON.parse(localStorage.getItem(key)) ?? {}; } catch { return {}; }
}
export function writeMap(key, map) {
  try { localStorage.setItem(key, JSON.stringify(map)); } catch { /* kept in memory only */ }
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

// Where this device read each chat to and where it left it (docs/web-client-chat.md "Opening
// a chat"), by the hook's `name`: `read` is the time of the newest incoming message that was
// on screen, `place` the first row in view and its distance from the log's top, or null at the end.
const READ_KEY = "wsx-read";
const READ_KEPT = 200;
function keepRead(name, entry) {
  const all = readMap(READ_KEY);
  // ^ The newest entries last; the oldest go first once there are too many.
  delete all[name];
  all[name] = entry;
  writeMap(READ_KEY, Object.fromEntries(Object.entries(all).slice(-READ_KEPT)));
}

// A conversation log that keeps its reader at the end only while the reader is at the end,
// and otherwise keeps the row the reader reads where it is on the screen
// (docs/web-client-chat.md "Following the end"). `log` is the scroller's ref callback; `toEnd`
// goes to the end and follows it (the jump button, the reader's own send); `away` shows the
// jump button. Nothing else moves a reader who scrolled up.
// ^ Rows change size after they render (a diagram draws, an image loads, an entry animates),
// rows come and go above the reader, and the box itself moves or changes size (the
// "Since you left" line, the reservoir, the keyboard): none of these fires a scroll event, and
// iOS Safari has no CSS scroll anchoring. So the log watches its box and each row, and settles
// after every change, before the browser paints.
// `name` keeps the reading per chat. Once `ready` (the first read is in), the log opens at its
// `.log-new` row when the component shows one, else where the reader left it, else at the end.
// Rows are the log's children: `data-key` names a row, `data-in` and `data-at` mark an incoming
// message and its time. `since` is the read time at opening, for the component's `.log-new`
// row; `unread` counts the incoming messages below the screen.
export function useFollowEnd(name, ready) {
  const node = useRef(null);
  const atEnd = useRef(true);
  const watchers = useRef([]);
  // The first row that starts in view and its distance from the top of the screen.
  const anchor = useRef(null);
  const height = useRef(0);
  // The scroll position after the last settle or scroll.
  const top = useRef(0);
  const [away, setAway] = useState(false);
  const [opened] = useState(() => readMap(READ_KEY)[name] ?? null);
  const placed = useRef(false);
  // ^ The observers keep the first render's functions: they read the props through refs.
  const loaded = useRef(ready);
  loaded.current = ready;
  const read = useRef(opened?.read ?? null);
  const [unread, setUnread] = useState(0);
  // ^ Layout positions, not getBoundingClientRect: a row's entry animation moves it with a
  // transform, which must not count as a move. `.log` is positioned, so offsetTop is in the log.
  const onScreen = (row) => node.current.getBoundingClientRect().top + row.offsetTop - node.current.scrollTop;
  const incoming = () => [...node.current.children].filter((row) => row.dataset.in != null && row.dataset.at);
  // ^ A message counts as read once its end was on a visible screen.
  const seen = () => {
    const element = node.current;
    const bottom = element.scrollTop + element.clientHeight + 1;
    const rows = incoming();
    if (document.visibilityState === "visible") {
      for (const row of rows) if (row.offsetTop + row.offsetHeight <= bottom) read.current = Math.max(read.current ?? 0, Number(row.dataset.at));
    }
    setUnread(rows.filter((row) => Number(row.dataset.at) > read.current).length);
  };
  const mark = () => {
    const element = node.current;
    // ^ Not the row cut by the top edge: when that row grows inside (its diagram draws), what
    // the reader sees of it is below the growth, and Chrome's CSS anchor there moves it too.
    const rows = [...element.children];
    const row = rows.find((child) => child.offsetTop >= element.scrollTop) ?? rows.at(-1);
    anchor.current = row ? { row, top: onScreen(row) } : null;
    top.current = element.scrollTop;
    if (placed.current) seen();
  };
  // ^ Read from the log as it is: a chat that closes still has its rows when its hooks clean up.
  const keep = () => {
    const element = node.current;
    if (!placed.current || !element) return;
    const named = !atEnd.current && [...element.children].find((child) => child.dataset.key && child.offsetTop >= element.scrollTop);
    keepRead(name, { read: read.current, place: named ? { key: named.dataset.key, offset: named.offsetTop - element.scrollTop } : null });
  };
  // The first placement, once the first read is in.
  const open = () => {
    const element = node.current;
    placed.current = true;
    // ^ A chat opened for the first time on this device has nothing unread.
    if (read.current == null) read.current = Math.max(0, ...incoming().map((row) => Number(row.dataset.at)));
    const fresh = element.querySelector(":scope > .log-new");
    const place = opened?.place;
    const left = place && [...element.children].find((row) => row.dataset.key === place.key);
    if (fresh) element.scrollTop = fresh.offsetTop - parseFloat(getComputedStyle(element).paddingTop);
    else if (left) element.scrollTop = left.offsetTop - place.offset;
    else element.scrollTop = element.scrollHeight;
    atEnd.current = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
    setAway(!atEnd.current);
    mark();
  };
  const settle = () => {
    const element = node.current;
    // A change may still arrive after the view closed.
    if (!element) return;
    height.current = element.clientHeight;
    if (!placed.current && loaded.current) return open();
    if (atEnd.current) element.scrollTop = element.scrollHeight;
    else if (anchor.current?.row.isConnected) {
      const moved = onScreen(anchor.current.row) - anchor.current.top;
      // ^ Smaller moves add up against the same mark until they come to a pixel.
      if (Math.abs(moved) < 1) return;
      element.scrollTop += moved;
    }
    mark();
  };
  const scrolled = () => {
    const element = node.current;
    if (!element) return;
    // ^ A box that changed height scrolls without the reader. The event can come before the
    // resize observer. A scroll farther than the change is the reader's, in the same frame.
    const resized = element.clientHeight - height.current;
    if (resized && Math.abs(element.scrollTop - top.current) <= Math.abs(resized) + 1) return settle();
    height.current = element.clientHeight;
    // ^ Only the reader scrolling up leaves the end. The event of the log's own pin can come
    // after a row grew again (a diagram drew), before the resize observer pins once more.
    if (atEnd.current && element.scrollTop >= top.current - 1) return settle();
    atEnd.current = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
    setAway(!atEnd.current);
    mark();
  };
  const log = useCallback((element) => {
    for (const watcher of watchers.current) watcher.disconnect();
    watchers.current = [];
    node.current = element;
    if (!element) return;
    height.current = element.clientHeight;
    const sizes = new ResizeObserver(settle);
    sizes.observe(element);
    for (const row of element.children) sizes.observe(row);
    const rows = new MutationObserver((changes) => {
      for (const change of changes) {
        for (const row of change.addedNodes) if (row.nodeType === Node.ELEMENT_NODE) sizes.observe(row);
        for (const row of change.removedNodes) if (row.nodeType === Node.ELEMENT_NODE) sizes.unobserve(row);
      }
      settle();
    });
    rows.observe(element, { childList: true });
    watchers.current = [sizes, rows];
  }, []);
  const toEnd = () => { atEnd.current = true; setAway(false); settle(); };
  useLayoutEffect(settle);
  // ^ Kept when the chat closes or the page goes to the background (a phone may drop it then);
  // a page that shows again counts what is on screen as read.
  useEffect(() => {
    const shown = () => {
      if (document.visibilityState === "hidden") keep();
      else if (node.current && placed.current) seen();
    };
    document.addEventListener("visibilitychange", shown);
    addEventListener("pagehide", keep);
    return () => {
      document.removeEventListener("visibilitychange", shown);
      removeEventListener("pagehide", keep);
      keep();
    };
  }, []);
  return { log, away, unread, since: opened?.read ?? null, scrolled, toEnd };
}

// Motion that script drives (a list that reorders, a view that leaves) reads the same tokens
// as the stylesheet and is off where the user asked for less motion (docs/web-client.md "Motion").
export function motion() {
  if (matchMedia("(prefers-reduced-motion: reduce)").matches) return null;
  const style = getComputedStyle(document.documentElement);
  const token = style.getPropertyValue("--motion-duration").trim();
  const duration = parseFloat(token) * (token.endsWith("ms") ? 1 : 1000);
  return duration > 0 ? { duration, easing: style.getPropertyValue("--motion-easing").trim() || "ease" } : null;
}

// One placeholder for a screen that waits for its data. Its words are for a screen reader.
export const Loading = () => html`
  <div class="loading" role="status">
    <span class="visually-hidden">Loading…</span>
    <span class="skeleton" aria-hidden="true"></span><span class="skeleton" aria-hidden="true"></span>
  </div>`;

// Holds `value` for one motion duration after it turns empty, so the view can fade out.
// Returns [value to show, true while it leaves].
export function useExit(value) {
  const last = useRef(value);
  const [, renew] = useState(0);
  if (value) last.current = value;
  useEffect(() => {
    const how = motion();
    if (value || !last.current) return undefined;
    if (!how) { last.current = null; return undefined; }
    const timer = setTimeout(() => { last.current = null; renew((count) => count + 1); }, how.duration);
    return () => clearTimeout(timer);
  }, [value]);
  return value ? [value, false] : [last.current, Boolean(last.current)];
}

// FLIP for a keyed list: rows glide to their new place, new rows fade in, and a row that
// went fades out where it was. Each child of `list` carries its key in `data-key`. The first
// render is left to the stylesheet's fade, so a screen arriving animates once.
export function useFlip(list) {
  const before = useRef(null);
  useLayoutEffect(() => {
    const parent = list.current;
    if (!parent) return;
    const how = motion();
    const rows = [...parent.children].filter((row) => !row.hasAttribute("data-ghost"));
    const now = new Map(rows.map((row) => [row.dataset.key, { top: row.offsetTop, row, copy: null }]));
    const past = before.current;
    if (how && past) {
      const timing = { duration: how.duration, easing: how.easing };
      for (const [key, entry] of now) {
        const old = past.get(key);
        if (!old) entry.row.animate([{ opacity: 0 }, { opacity: 1 }], timing);
        else if (old.top !== entry.top) entry.row.animate([{ transform: `translateY(${old.top - entry.top}px)` }, { transform: "none" }], timing);
      }
      for (const [key, old] of past) {
        if (now.has(key)) continue;
        const ghost = old.copy;
        ghost.setAttribute("data-ghost", "");
        ghost.setAttribute("aria-hidden", "true");
        parent.append(ghost);
        const at = `translateY(${old.top}px)`;
        ghost.animate([{ transform: at, opacity: 1 }, { transform: at, opacity: 0 }], timing).finished.then(() => ghost.remove(), () => ghost.remove());
      }
    }
    for (const entry of now.values()) entry.copy = how ? entry.row.cloneNode(true) : null;
    before.current = now;
  });
}
