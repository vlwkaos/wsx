// Reply to one message in the session chat and the manager chat (docs/web-client-chat.md
// "Replies"): a swipe right on a message on a touch screen (a right click with a mouse) opens
// a menu, and Reply puts a one-line quote above the composer; the sent text starts with
// `> <that line>`, and a tap on the quote in the chat scrolls to the quoted message while it is still in the log; a quote with
// the message's id (`quoted_id`, sent as `> message <id>`) opens the chat there when it is not.
// A row that can be quoted is a child of the log with `data-quote`, its first line as the
// server cuts it (wsx-core `quote_of`, sent as `quote`).
import { html, Icon, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";
import { sheetHash } from "./store.js";
import { toast } from "./ui.js";
import { commits, speedAt, swipes, track } from "./swipe.js";

// A swipe this far right opens the menu (swipe.js `commits`: or a flick, or the finger stays
// past it). A tap or a long press is the page's own and the system's (a link, a control, the
// text selection): the page adds nothing to them (no `user-select: none`, no menu).
const SWIPE_PX = 64;
const HINT_PX = 20;
// ^ Pressing these keeps their own meaning: a link, a control, or a block that scrolls sideways.
const OWN = "a, button, input, textarea, summary, pre, .table-wrap, .diagram";

// The text after a leading quote line, and that line (or null).
export function splitQuote(text) {
  const quoted = /^> (.*)\n\n?/.exec(text ?? "");
  return quoted ? [quoted[1], text.slice(quoted[0].length)] : [null, text ?? ""];
}

export const withQuote = (quote, body) => quote ? `> ${quote}\n\n${body}` : body;

// A message as a chat row's fields: `quoted`, its leading quote line, and `text`, the rest.
export function quoteSplit(text) {
  const [quoted, rest] = splitQuote(text);
  return { quoted, text: rest.trim() };
}

// A message of the log marked as found for two seconds.
export function markFound(target) {
  target.dataset.found = "";
  setTimeout(() => delete target.dataset.found, 2000);
}

// A quote as a button that goes to the quoted message. `id`: the quoted message's id, when the
// log stored it; the message is found by it, and when it is not among the rows the page asks
// for the chat around it (`quote-jump`, tasks.js). Without an id the newest message above with
// that first line. `check`: the To check item (`c<n>`) the quote is the check of; its bubble has
// that `data-id`, and when it is not among the rows the To check list opens at it.
export function Quote({ quote, id = null, check = null, project = null }) {
  const go = (event) => {
    const row = event.currentTarget.closest("[data-quote]");
    const rows = row && [...row.parentElement.children];
    const target = check != null || id != null
      ? rows?.find((other) => other.dataset.id === String(check ?? id))
      : rows?.filter((other) => other.dataset.quote === quote && (other.compareDocumentPosition(row) & Node.DOCUMENT_POSITION_FOLLOWING)).at(-1);
    if (!target && check != null) {
      location.hash = sheetHash(project, "checks", check);
      return;
    }
    if (!target && id != null) {
      dispatchEvent(new CustomEvent("quote-jump", { detail: id }));
      return;
    }
    if (!target) {
      toast("That message is no longer in the conversation shown here.");
      return;
    }
    // ^ The log owns its scroll (`useFollowEnd`, lib.js `reveal`): it moves there and leaves the end in the same task.
    dispatchEvent(new CustomEvent("quote-show", { detail: target }));
    markFound(target);
  };
  return html`<button class="quote" type="button" aria-label=${`Go to the quoted message: ${quote}`} onClick=${go}>${quote}</button>`;
}

// A sent message: its quote, then its words.
export function Quoted({ text, words }) {
  const [quote, rest] = splitQuote(text);
  return html`
    ${quote != null && html`<${Quote} quote=${quote} />`}
    ${rest && html`<p class="prose">${words ? words(rest) : rest}</p>`}`;
}

// The quote over the composer, with its remove (×).
export const QuoteChip = ({ quote, onRemove }) => quote ? html`
  <div class="reply-quote">
    <span class="reply-quote-text"><span class="visually-hidden">Replying to: </span>${quote}</span>
    <button class="icon-button" type="button" title="Remove" aria-label="Remove the quote" onClick=${onRemove}><${Icon} name="fail" /></button>
  </div>` : null;

// The log's swipe and right click and its menu. `onReply(quote, id)` takes a row's quote and its
// id (a string, or undefined). Returns the handlers for the log element and the menu (with the
// swipe's icon) to render beside it.
export function useReply(onReply) {
  const gesture = useRef(null);
  const hint = useRef();
  const [menu, setMenu] = useState(null);
  const row = (event) => {
    const found = event.target.closest?.("[data-quote]");
    return found?.parentElement === event.currentTarget && found.dataset.quote && !event.target.closest(OWN) ? found : null;
  };
  const end = () => {
    const now = gesture.current;
    if (!now) return;
    if (now.swiping) {
      now.row.style.translate = "";
      delete now.row.dataset.swiping;
      hint.current.hidden = true;
    }
    gesture.current = null;
  };
  // ^ The swipe counts at SWIPE_PX, as a flick, or while the finger stays past it (swipe.js).
  const opens = (now, time) => commits(now.dx, SWIPE_PX, { speed: speedAt(now.trail, time), past: now.past });
  const handlers = {
    onPointerDown: (event) => {
      end();
      const found = event.button === 0 && row(event);
      // ^ A mouse selects with its drag and opens the menu with a right click: only a finger swipes.
      if (!found || event.pointerType === "mouse") return;
      gesture.current = { row: found, id: event.pointerId, x: event.clientX, y: event.clientY, swiping: false, past: false, trail: [] };
    },
    onPointerMove: (event) => {
      const now = gesture.current;
      if (now?.id !== event.pointerId) return;
      const dx = event.clientX - now.x;
      const dy = event.clientY - now.y;
      if (!now.swiping) {
        // ^ A finger that is not yet clearly sideways and to the right waits: a vertical drag is the
        // log's scroll and the browser ends it with a pointercancel; one that never turns is a tap.
        if (!swipes(dx, dy)) return;
        now.swiping = true;
        now.row.dataset.swiping = "";
        now.box = now.row.getBoundingClientRect();
        now.log = now.row.parentElement.getBoundingClientRect();
      }
      now.dx = dx;
      now.past ||= dx >= SWIPE_PX;
      now.trail = track(now.trail, event.timeStamp, dx);
      const ready = opens(now, event.timeStamp);
      const shown = Math.max(0, Math.min(dx, SWIPE_PX * 1.25));
      now.row.style.translate = `${shown}px 0`;
      // ^ The icon of the action (Reply, as the menu names it) in the gap the row leaves: it
      // grows and fades in with the drag and is solid, in the accent, while the release would open the menu. It stays
      // beside the finger, inside the row and the log.
      const node = hint.current;
      const progress = ready ? 1 : Math.min(shown / SWIPE_PX, 1);
      const top = Math.max(now.box.top, now.log.top) + HINT_PX;
      const middle = Math.max(top, Math.min(event.clientY, Math.min(now.box.bottom, now.log.bottom) - HINT_PX));
      node.hidden = false;
      node.toggleAttribute("data-ready", ready);
      node.style.left = `${now.box.left + shown / 2 - HINT_PX / 2}px`;
      node.style.top = `${middle - HINT_PX / 2}px`;
      node.style.opacity = String(progress);
      node.style.scale = String(0.5 + progress / 2);
    },
    onPointerUp: (event) => {
      const now = gesture.current;
      if (now?.id !== event.pointerId) return;
      const opened = now.swiping && opens(now, event.timeStamp);
      end();
      if (opened) setMenu({ row: now.row, x: event.clientX, y: event.clientY });
    },
    onPointerCancel: end,
    // ^ A swipe owns its touch from the moment it starts: a cancelled touchmove leaves the browser no scroll to
    // take, so the thumb's arc turning down does not end the swipe with a pointercancel (iOS Safari takes the
    // vertical pan late, Chrome decides the direction once at the start). Preact adds it non-passive.
    onTouchMove: (event) => {
      if (gesture.current?.swiping && event.cancelable) event.preventDefault();
    },
    onContextMenu: (event) => {
      const found = event.pointerType !== "touch" && row(event);
      if (!found) return;
      event.preventDefault();
      end();
      setMenu({ row: found, x: event.clientX, y: event.clientY });
    },
  };
  return [handlers, html`
    <span class="swipe-hint" ref=${hint} hidden><${Icon} name="reply" /></span>
    ${menu && html`<${Menu} menu=${menu} onReply=${onReply} onClose=${() => setMenu(null)} />`}`];
}

// Reply and Copy beside the swiped message. A tap outside, Escape or a scroll closes it.
function Menu({ menu, onReply, onClose }) {
  const node = useRef();
  useLayoutEffect(() => {
    const box = node.current.getBoundingClientRect();
    node.current.style.left = `${Math.max(0, Math.min(menu.x, innerWidth - box.width))}px`;
    node.current.style.top = `${Math.max(0, Math.min(menu.y, innerHeight - box.height))}px`;
    node.current.querySelector("button").focus({ preventScroll: true });
  }, [menu]);
  useEffect(() => {
    const outside = (event) => { if (!node.current?.contains(event.target)) onClose(); };
    const key = (event) => { if (event.key === "Escape") onClose(); };
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("keydown", key);
    document.addEventListener("scroll", onClose, true);
    return () => {
      document.removeEventListener("pointerdown", outside, true);
      document.removeEventListener("keydown", key);
      document.removeEventListener("scroll", onClose, true);
    };
  }, []);
  const copy = async () => {
    onClose();
    const words = menu.row.querySelector(".prose, .markdown")?.innerText ?? menu.row.dataset.quote;
    try {
      await navigator.clipboard.writeText(words);
      toast("Copied");
    } catch {
      toast("Copy failed");
    }
  };
  return html`
    <div class="reply-menu" role="menu" aria-label="Message" ref=${node}>
      <button role="menuitem" type="button" onClick=${() => { onClose(); onReply(menu.row.dataset.quote, menu.row.dataset.id); }}><${Icon} name="reply" />Reply</button>
      <button role="menuitem" type="button" onClick=${copy}><${Icon} name="copy" />Copy</button>
    </div>`;
}
