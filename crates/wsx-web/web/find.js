// Find in a chat: a bar under the top row that marks each match of the typed text in the
// conversation, case-insensitive, and scrolls to the current one. It starts at the newest
// match; Enter and ↑ go to the older one, Shift+Enter and ↓ to the newer one, Esc closes.
// The page loads the whole history while the bar is open (docs/web-client-chat.md "Find").
import { html, Icon, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";

// ^ CSS Custom Highlight API: the marks paint over the rendered text, so the conversation's
// DOM stays as Preact and the markdown renderer wrote it. A browser without it still counts
// and scrolls.
const marks = globalThis.CSS?.highlights && globalThis.Highlight ? CSS.highlights : null;

export const FindButton = ({ open, onClick }) => html`
  <button class="icon-button" type="button" aria-label="Find in chat" title="Find" aria-pressed=${open} onClick=${onClick}><${Icon} name="search" /></button>`;

// The text ranges in `log` that match `query`, in reading order. Text in a closed
// <details>, other than its summary, is not on the screen and does not count.
function find(log, query) {
  const pattern = new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "gi");
  const walker = document.createTreeWalker(log, NodeFilter.SHOW_TEXT);
  const ranges = [];
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const closed = node.parentElement.closest("details:not([open])");
    if (closed && !node.parentElement.closest("summary")) continue;
    for (const match of node.data.matchAll(pattern)) {
      const range = new Range();
      range.setStart(node, match.index);
      range.setEnd(node, match.index + match[0].length);
      ranges.push(range);
    }
  }
  return ranges;
}

// `log`: the conversation's node. `loading`: the whole history is not in yet.
export function FindBar({ log, loading, onClose }) {
  const [query, setQuery] = useState("");
  // ^ The current match from the oldest; null starts at the newest.
  const [at, setAt] = useState(null);
  const [ranges, setRanges] = useState([]);
  const count = ranges.length;
  const field = useRef(null);
  const before = useRef(null);
  const current = at == null ? count - 1 : Math.min(at, count - 1);

  useLayoutEffect(() => {
    before.current = document.activeElement;
    field.current?.focus();
    // ^ Closing gives the focus back to what had it, as a menu does.
    return () => {
      marks?.delete("find");
      marks?.delete("find-current");
      if (document.activeElement === document.body || !document.activeElement?.isConnected) before.current?.focus?.();
    };
  }, []);

  // Marks again when the query changes and when rows arrive or open (a fold, Show earlier).
  useEffect(() => {
    const node = log.current;
    if (!node) return undefined;
    let frame = 0;
    const mark = () => {
      frame = 0;
      const found = query ? find(node, query) : [];
      setRanges(found);
      marks?.set("find", new Highlight(...found));
    };
    mark();
    const observer = new MutationObserver(() => { frame ||= requestAnimationFrame(mark); });
    observer.observe(node, { childList: true, subtree: true, characterData: true });
    return () => { observer.disconnect(); cancelAnimationFrame(frame); };
  }, [query, log.current]);

  // The current match has its own mark, set in the same frame as the count and again on
  // each re-mark, because a re-render can replace the text node of the old range.
  useLayoutEffect(() => {
    const range = ranges[current];
    if (range) marks?.set("find-current", new Highlight(range));
    else marks?.delete("find-current");
  }, [ranges, current]);
  // ^ It scrolls to the middle of the log only when the reader moves or the query changes,
  // not when rows arrive.
  useLayoutEffect(() => {
    ranges[current]?.startContainer.parentElement?.scrollIntoView({ block: "center" });
  }, [current, count, query]);

  // ^ Not while rows still go in above: the count from the oldest would move under the reader.
  const step = (delta) => count && !loading && setAt((current + delta + count) % count);
  const key = (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      onClose();
    } else if (event.key === "Enter" || event.key === "ArrowUp" || event.key === "ArrowDown") {
      event.preventDefault();
      step(event.key === "ArrowDown" || (event.key === "Enter" && event.shiftKey) ? 1 : -1);
    }
  };
  const words = !query ? "" : loading ? "…" : `${count ? current + 1 : 0}/${count}`;
  return html`
    <div class="find" role="search">
      <input class="input find-field" ref=${field} type="search" aria-label="Find in chat" placeholder="Find"
        autocomplete="off" enterkeyhint="search" value=${query} onKeyDown=${key}
        onInput=${(event) => { setQuery(event.currentTarget.value); setAt(null); }} />
      <span class="find-count" aria-live="polite">${words}</span>
      <button class="icon-button" type="button" aria-label="Previous match" disabled=${!count || loading} onClick=${() => step(-1)}><${Icon} name="up" /></button>
      <button class="icon-button" type="button" aria-label="Next match" disabled=${!count || loading} onClick=${() => step(1)}><${Icon} name="down" /></button>
      <button class="icon-button" type="button" aria-label="Close find" onClick=${onClose}><${Icon} name="fail" /></button>
    </div>`;
}
