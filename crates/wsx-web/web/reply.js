// Reply to one message in the session chat and the manager chat (docs/web-client-chat.md
// "Replies"): a swipe right on a message, or a long press (a right click) and Reply, puts a
// one-line quote above the composer; the sent text starts with `> <that line>`, and a tap on
// the quote in the chat scrolls to the quoted message while it is still in the log.
// A row that can be quoted is a child of the log with `data-quote`, its first line.
import { html, Icon, motion, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";
import { toast } from "./ui.js";

const QUOTE_CHARS = 120;
// A swipe this far right replies; a press this long opens the menu; a move past the slop is
// a scroll or a swipe, not a press.
const SWIPE_PX = 64;
const PRESS_MS = 450;
const SLOP_PX = 10;
// ^ Pressing these keeps their own meaning: a link, a control, or a block that scrolls sideways.
const OWN = "a, button, input, textarea, summary, pre, .table-wrap, .diagram";

// The text after a leading quote line, and that line (or null).
function splitQuote(text) {
  const quoted = /^> (.*)\n\n?/.exec(text ?? "");
  return quoted ? [quoted[1], text.slice(quoted[0].length)] : [null, text ?? ""];
}

// A message's first line without its own quote, cut to QUOTE_CHARS.
export function quoteOf(text) {
  const line = splitQuote(text)[1].split("\n").map((part) => part.trim()).find(Boolean) ?? "";
  return line.length > QUOTE_CHARS ? `${line.slice(0, QUOTE_CHARS - 1).trimEnd()}…` : line;
}

export const withQuote = (quote, body) => quote ? `> ${quote}\n\n${body}` : body;

// A sent message: its quote as a button that goes to the quoted message, then its words.
export function Quoted({ text, words }) {
  const [quote, rest] = splitQuote(text);
  const go = (event) => {
    const row = event.currentTarget.closest("[data-quote]");
    // ^ The newest message above this one that starts with the quoted line.
    const target = row && [...row.parentElement.children]
      .filter((other) => other.dataset.quote === quote && (other.compareDocumentPosition(row) & Node.DOCUMENT_POSITION_FOLLOWING))
      .at(-1);
    if (!target) {
      toast("That message is no longer in the conversation shown here.");
      return;
    }
    target.scrollIntoView({ block: "center", behavior: motion() ? "smooth" : "auto" });
    target.dataset.found = "";
    setTimeout(() => delete target.dataset.found, 2000);
  };
  return html`
    ${quote != null && html`<button class="quote" type="button" aria-label=${`Go to the quoted message: ${quote}`} onClick=${go}>${quote}</button>`}
    ${rest && html`<p class="prose">${words ? words(rest) : rest}</p>`}`;
}

// The quote over the composer, with its remove (×).
export const QuoteChip = ({ quote, onRemove }) => quote ? html`
  <div class="reply-quote">
    <span class="reply-quote-text"><span class="visually-hidden">Replying to: </span>${quote}</span>
    <button class="icon-button" type="button" title="Remove" aria-label="Remove the quote" onClick=${onRemove}><${Icon} name="fail" /></button>
  </div>` : null;

// The log's gestures and its menu. `onReply(quote)` takes a row's quote. Returns the handlers
// for the log element and the menu to render beside it.
export function useReply(onReply) {
  const gesture = useRef(null);
  const [menu, setMenu] = useState(null);
  const row = (event) => {
    const found = event.target.closest?.("[data-quote]");
    return found?.parentElement === event.currentTarget && found.dataset.quote && !event.target.closest(OWN) ? found : null;
  };
  const end = () => {
    const now = gesture.current;
    if (!now) return;
    clearTimeout(now.timer);
    if (now.swiping) {
      now.row.style.translate = "";
      delete now.row.dataset.swiping;
    }
    gesture.current = null;
  };
  const handlers = {
    onPointerDown: (event) => {
      end();
      const found = event.button === 0 && row(event);
      if (!found) return;
      const open = () => { setMenu({ row: found, x: event.clientX, y: event.clientY }); end(); };
      gesture.current = { row: found, id: event.pointerId, x: event.clientX, y: event.clientY, touch: event.pointerType !== "mouse", swiping: false, timer: setTimeout(open, PRESS_MS) };
    },
    onPointerMove: (event) => {
      const now = gesture.current;
      if (now?.id !== event.pointerId) return;
      const dx = event.clientX - now.x;
      const dy = event.clientY - now.y;
      if (!now.swiping) {
        if (Math.hypot(dx, dy) < SLOP_PX) return;
        clearTimeout(now.timer);
        // ^ Mostly sideways and to the right is a swipe; anything else is the log's scroll.
        if (!now.touch || dx < SLOP_PX || dx < Math.abs(dy) * 2) { gesture.current = null; return; }
        now.swiping = true;
        now.row.dataset.swiping = "";
      }
      now.dx = dx;
      now.row.style.translate = `${Math.max(0, Math.min(dx, SWIPE_PX * 1.25))}px 0`;
    },
    onPointerUp: (event) => {
      const now = gesture.current;
      if (now?.id !== event.pointerId) return;
      const replied = now.swiping && now.dx >= SWIPE_PX;
      end();
      if (replied) onReply(now.row.dataset.quote);
    },
    onPointerCancel: end,
    onContextMenu: (event) => {
      const found = row(event);
      if (!found) return;
      event.preventDefault();
      end();
      setMenu({ row: found, x: event.clientX, y: event.clientY });
    },
  };
  return [handlers, menu && html`<${Menu} menu=${menu} onReply=${onReply} onClose=${() => setMenu(null)} />`];
}

// Reply and Copy beside the pressed message. A tap outside, Escape or a scroll closes it.
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
      <button role="menuitem" type="button" onClick=${() => { onClose(); onReply(menu.row.dataset.quote); }}><${Icon} name="reply" />Reply</button>
      <button role="menuitem" type="button" onClick=${copy}><${Icon} name="copy" />Copy</button>
    </div>`;
}
