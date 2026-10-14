// Terminal view of one session: xterm.js on the pane's raw byte stream over one WebSocket.
// Contract: docs/web-client.md "Terminal". Input behaviour follows remotty's Terminal.tsx.
// ^ wsxd's own emulator already answers terminal queries, so this one must stay silent.
import { resolve } from "./api.js";
import { add, fill, h } from "./dom.js";
import { can, state as store, touch as touchDevice } from "./store.js";

const KEYS = [
  ["Esc", "\x1b", "Escape"], ["Tab", "\t", "Tab"], ["^C", "\x03", "Control C"], ["^V", "\x16", "Control V"],
  ["^A", "\x01", "Control A"], ["D", "d", "D"], ["^Z", "\x1a", "Control Z"], ["^R", "\x12", "Control R"],
  ["^L", "\x0c", "Control L"], ["⌫", "\x7f", "Backspace"],
];
// ^ Owner's rule: Enter sits at the right edge of the second row, under the thumb, with ^J
// (a new line) just left of it. Neither repeats when held: one press is one line.
const LINE_KEYS = [["^J", "\x0a", "Control J, new line"], ["Enter", "\r", "Enter"]];
const REPEAT_MS = 80;
const ARROW_STEP = 18; // pixels of drag per arrow key
const TOUCH_SCROLL_STEP = 28; // pixels of touch movement per line
const SCROLLBACK = 10000;
const FONT_PHONE = 11;
const FONT_DESKTOP = 14;
const REPLAY = "\x1bc"; // full reset, written in the stream so it stays in order with queued bytes
const RETRY_MS = [500, 1000, 2000, 5000];
const MAX_FRAME = 32 * 1024;
const SIZE_KEY = "wsx-terminal-size";
const SVG = "http://www.w3.org/2000/svg";
const ICONS = {
  fit: "M3 8V3h5M17 8V3h-5M3 12v5h5M17 12v5h-5",
  host: "M3 4h14v12H3zM7 8h6v4H7z",
};
// A line icon. Its button carries the accessible name.
function icon(name) {
  const svg = document.createElementNS(SVG, "svg");
  const attributes = { viewBox: "0 0 20 20", width: "20", height: "20", fill: "none", stroke: "currentColor",
    "stroke-width": "1.75", "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" };
  for (const [key, value] of Object.entries(attributes)) svg.setAttribute(key, value);
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("d", ICONS[name]);
  svg.append(path);
  return svg;
}
const RETRYABLE = new Set(["daemon_handoff", "stream_disconnected", "daemon_unavailable", "daemon_replacing"]);

// Stable while this page is open, so a reconnect or a reopened panel replaces the
// page's own old stream without a takeover. Another tab is another controller.
const client = crypto.getRandomValues(new Uint32Array(2)).join("");

let library;
// xterm.js is loaded only when a terminal opens; a chat-only visit never fetches it.
function loadLibrary() {
  library ??= new Promise((done, failed) => {
    // From this module's own build path (docs/web-client.md "Build").
    const asset = (name) => new URL(name, import.meta.url).href;
    add(document.head, h("link", { rel: "stylesheet", href: asset("xterm.css") }));
    add(document.head, h("link", { rel: "stylesheet", href: asset("terminal-fonts.css") }));
    const next = ([name, ...rest]) => {
      if (!name) return done();
      const script = h("script", { src: asset(name) });
      script.onload = () => next(rest);
      script.onerror = () => { library = undefined; failed(new Error("Cannot load the terminal. Check the connection.")); };
      add(document.head, script);
    };
    next(["xterm.js", "xterm-addon-fit.js"]);
  });
  return library;
}

function remembered() {
  try { return localStorage.getItem(SIZE_KEY); } catch { return null; }
}
function remember(value) {
  try { localStorage.setItem(SIZE_KEY, value); } catch { /* a private window keeps the default */ }
}

// Runs of these characters may be one ligature in the terminal font (=> != -> === <=).
// xterm.js draws each run as one piece of text, and the font decides what it joins.
// The cells, the cursor, the selection and copied text stay one character each.
const LIGATURE_RUNS = /[-=<>!|&:+*/.#?~^%]{2,8}|www/g;
function joinLigatures(term) {
  term.registerCharacterJoiner((text) => [...text.matchAll(LIGATURE_RUNS)].map((match) => [match.index, match.index + match[0].length]));
}

// The shipped fonts must be loaded before xterm.js measures its cell, or the grid has
// the size of the fallback font. A font that is late is measured again when it arrives.
async function terminalFonts(family, size) {
  const faces = [`${size}px ${family}`, `bold ${size}px ${family}`, `italic ${size}px ${family}`].map((font) => document.fonts.load(font, "A\ue0b0"));
  await Promise.race([Promise.allSettled(faces), new Promise((done) => setTimeout(done, 3000))]);
}

// Queries that xterm.js would answer on its own. wsxd answered them already.
function silenceQueries(term) {
  const swallow = () => true;
  const csi = [{ final: "c" }, { prefix: ">", final: "c" }, { final: "n" }, { prefix: "?", final: "n" },
    { intermediates: "$", final: "p" }, { prefix: "?", intermediates: "$", final: "p" }];
  for (const id of csi) term.parser.registerCsiHandler(id, swallow);
  term.parser.registerDcsHandler({ intermediates: "$", final: "q" }, swallow);
  for (const code of [4, 10, 11, 12]) term.parser.registerOscHandler(code, (data) => data.includes("?"));
}

// `mount` receives the view, `current` is the session id, and `tools` is the place in
// the session's top row for this view's size toggle.
export function terminalView(mount, current, tools) {
  let closed = false;
  const canType = can("terminal_input");
  const touch = touchDevice();
  // A device that may not type never resizes the pane: it scales the host's size.
  let hostSize = !canType || remembered() === "host";
  const encoder = new TextEncoder();

  const disposers = [];
  const listen = (target, type, handler, options) => {
    target.addEventListener(type, handler, options);
    disposers.push(() => target.removeEventListener(type, handler, options));
  };
  let term = null;
  let fit = null;
  let socket = null;
  let generation = 0;
  let retries = 0;
  let retryTimer = 0;
  let state = "loading";

  const sizeToggle = h("button", { type: "button", class: "icon-button terminal-size", onclick: toggleSize });
  const noticeText = h("span", {});
  const noticeAction = h("button", { type: "button", class: "button-primary", hidden: true });
  const notice = h("div", { class: "terminal-notice", role: "status", hidden: true }, noticeText, noticeAction);
  const host = h("div", { class: "terminal-host" });
  const stage = h("div", { class: "terminal-stage" }, notice, host);
  const keys = touch && canType ? keyRows() : null;
  // The frame around the screen is dark in either scheme (docs/web-themes.md).
  const root = h("div", { class: "terminal", "data-state": state, "data-scheme": "dark" }, stage, keys);
  add(mount, root);
  if (canType) add(tools, sizeToggle);

  function setState(next) {
    state = next;
    root.dataset.state = next;
  }

  // One line above the screen. `action` is [label, handler] for a decision the reader must take.
  function say(text, action) {
    notice.hidden = !text;
    noticeText.textContent = text ?? "";
    noticeText.title = text ?? "";
    noticeAction.hidden = !action;
    if (action) {
      noticeAction.textContent = action[0];
      noticeAction.onclick = action[1];
    }
  }

  function send(bytes) {
    if (!canType || state !== "live" || socket?.readyState !== WebSocket.OPEN) return;
    for (let start = 0; start < bytes.length; start += MAX_FRAME) socket.send(bytes.subarray(start, start + MAX_FRAME));
  }
  const sendText = (text) => send(encoder.encode(text));
  function sendControl(message) {
    if (state === "live" && socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
  }

  function updateSizeToggle() {
    const label = hostSize
      ? "Host size: the pane keeps its size and the view is scaled. Switch to fit this screen."
      : "Fit screen: the pane is resized to this screen. Switch to host size.";
    fill(sizeToggle, icon(hostSize ? "host" : "fit"));
    sizeToggle.dataset.size = hostSize ? "host" : "fit";
    sizeToggle.setAttribute("aria-label", label);
    sizeToggle.title = label;
  }
  function toggleSize() {
    hostSize = !hostSize;
    remember(hostSize ? "host" : "fit");
    updateSizeToggle();
    layout();
    connect();
  }

  // Fit mode: the pane follows this screen. Host mode: the screen is scaled to show the pane.
  function layout() {
    if (!term || closed) return;
    if (!hostSize) {
      delete host.dataset.size;
      host.style.transform = "";
      fit.fit();
      return;
    }
    host.dataset.size = "host";
    const screen = host.querySelector(".xterm-screen");
    if (!screen?.offsetWidth) return;
    const scale = Math.min(1, stage.clientWidth / screen.offsetWidth, stage.clientHeight / screen.offsetHeight);
    host.style.transform = `scale(${scale})`;
  }

  function retryLater(text) {
    setState("reconnecting");
    say(text);
    clearTimeout(retryTimer);
    retryTimer = setTimeout(connect, RETRY_MS[Math.min(retries++, RETRY_MS.length - 1)]);
  }

  function connect({ takeover = false } = {}) {
    clearTimeout(retryTimer);
    const mine = ++generation;
    socket?.close();
    socket = null;
    if (closed || !term || !current) return;
    setState("connecting");
    const url = new URL(`api/terminal/${current}/ws`, document.baseURI);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    url.searchParams.set("client", client);
    if (takeover) url.searchParams.set("takeover", "true");
    if (!hostSize) {
      fit.fit();
      url.searchParams.set("rows", term.rows);
      url.searchParams.set("cols", term.cols);
    }
    const link = new WebSocket(url);
    link.binaryType = "arraybuffer";
    socket = link;
    // Set when a control message already explained why the stream ended.
    let explained = false;
    link.onmessage = (event) => {
      if (mine !== generation) return;
      if (typeof event.data !== "string") {
        term.write(new Uint8Array(event.data));
        return;
      }
      const message = JSON.parse(event.data);
      switch (message.type) {
        case "replay":
          retries = 0;
          setState("live");
          say(message.truncated ? "Older scrollback was left out." : null);
          term.write(REPLAY, () => {
            if (hostSize) term.resize(message.cols, message.rows);
            else if (term.rows !== message.rows || term.cols !== message.cols) sendControl({ type: "resize", rows: term.rows, cols: term.cols });
            layout();
          });
          if (!touch) term.focus();
          break;
        case "resized":
          if (hostSize) term.write("", () => { term.resize(message.cols, message.rows); layout(); });
          break;
        case "clipboard":
          navigator.clipboard?.writeText(message.text).catch(() => say("The program copied text, but this browser refused the clipboard."));
          break;
        case "busy":
        case "control_lost":
          explained = true;
          setState("elsewhere");
          say(
            message.type === "busy" ? "Controlled from another wsx window." : "Control moved to another wsx window.",
            canType ? ["Take control", () => connect({ takeover: true })] : ["Try again", () => connect()],
          );
          break;
        case "exited":
          explained = true;
          setState("exited");
          say("The program in this session exited.");
          break;
        case "revoked":
          location.reload();
          break;
        case "error":
          explained = true;
          if (RETRYABLE.has(message.code)) retryLater("wsx is restarting on the host. Reconnecting…");
          else {
            setState("failed");
            say(message.message, ["Try again", () => connect({ takeover: canType })]);
          }
          break;
      }
    };
    link.onclose = () => {
      if (mine !== generation || closed || explained) return;
      retryLater("Connection lost. Reconnecting…");
    };
  }

  function keyRows() {
    let repeat = 0;
    const stop = () => { clearInterval(repeat); repeat = 0; };
    // preventDefault on pointerdown keeps focus, and the on-screen keyboard, on the terminal.
    const keyButton = ([label, sequence, name], repeats) => h("button", {
      type: "button", tabIndex: -1, class: "terminal-key", "aria-label": name,
      onpointerdown: (event) => {
        event.preventDefault();
        stop();
        sendText(sequence);
        if (repeats) repeat = setInterval(() => sendText(sequence), REPEAT_MS);
      },
      onpointerup: stop, onpointerleave: stop, onpointercancel: stop,
    }, label);
    const row = h("div", { class: "terminal-keys", role: "group", "aria-label": "Terminal keys" },
      KEYS.map((key) => keyButton(key, true)));
    // A fade marks each edge of the row that has more keys.
    const mark = () => {
      const more = [];
      if (row.scrollLeft > 1) more.push("start");
      if (row.scrollLeft + row.clientWidth < row.scrollWidth - 1) more.push("end");
      row.dataset.more = more.join(" ");
    };
    row.addEventListener("scroll", mark, { passive: true });
    const rowSize = new ResizeObserver(mark);
    rowSize.observe(row);
    disposers.push(() => rowSize.disconnect());
    const paste = h("button", {
      type: "button", tabIndex: -1, class: "terminal-key", "aria-live": "polite",
      onpointerdown: (event) => event.preventDefault(),
      onclick: async () => {
        try {
          const text = await navigator.clipboard.readText();
          if (text) term?.paste(text);
          paste.textContent = "Paste";
          paste.removeAttribute("data-failed");
        } catch {
          // Stays visible until a later paste works; the reader can tap again.
          paste.textContent = "Paste failed";
          paste.dataset.failed = "";
        }
      },
    }, "Paste");
    const drag = { ax: 0, ay: 0, lx: 0, ly: 0 };
    const pad = h("div", {
      class: "terminal-pad", role: "img", "aria-label": "Drag to send arrow keys",
      onpointerdown: (event) => {
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
        Object.assign(drag, { ax: 0, ay: 0, lx: event.clientX, ly: event.clientY });
      },
      onpointermove: (event) => {
        if (!(event.buttons & 1)) return;
        drag.ax += event.clientX - drag.lx;
        drag.ay += event.clientY - drag.ly;
        drag.lx = event.clientX;
        drag.ly = event.clientY;
        while (drag.ax >= ARROW_STEP) { sendText("\x1b[C"); drag.ax -= ARROW_STEP; }
        while (drag.ax <= -ARROW_STEP) { sendText("\x1b[D"); drag.ax += ARROW_STEP; }
        while (drag.ay >= ARROW_STEP) { sendText("\x1b[B"); drag.ay -= ARROW_STEP; }
        while (drag.ay <= -ARROW_STEP) { sendText("\x1b[A"); drag.ay += ARROW_STEP; }
      },
    }, "← ↑ ↓ →");
    return h("div", { class: "terminal-controls" }, row,
      h("div", { class: "terminal-tools" },
        h("div", {}, typeof navigator.clipboard?.readText === "function" ? paste : null), pad,
        h("div", { class: "terminal-line-keys", role: "group", "aria-label": "Line keys" }, LINE_KEYS.map((key) => keyButton(key, false)))));
  }

  // Wheel report to a program that tracks the mouse. xterm.js has no public call for it.
  function sendWheel(direction, clientX, clientY) {
    const rect = host.getBoundingClientRect();
    const x = Math.max(0, Math.floor(clientX - rect.left));
    const y = Math.max(0, Math.floor(clientY - rect.top));
    term._core?.coreMouseService?.triggerMouseEvent({
      col: Math.min(term.cols - 1, Math.floor((x / rect.width) * term.cols)),
      row: Math.min(term.rows - 1, Math.floor((y / rect.height) * term.rows)),
      x, y, button: 4, action: direction < 0 ? 0 : 1,
    });
  }

  function touchInput() {
    const textarea = term.textarea;
    let composing = false;
    const isComposing = (event) => composing || event.isComposing || event.keyCode === 229;
    listen(textarea, "compositionstart", () => { composing = true; });
    listen(textarea, "compositionend", () => { composing = false; });
    // iOS: without this xterm.js sends the backspace twice. Composition events are
    // left alone so Korean and other IMEs can edit their preedit text.
    listen(textarea, "keydown", (event) => {
      if (isComposing(event)) return;
      if (event.key === "Backspace" || event.keyCode === 8) event.stopPropagation();
    }, { capture: true });
    listen(textarea, "beforeinput", (event) => {
      if (isComposing(event) || event.inputType !== "deleteContentBackward") return;
      event.preventDefault();
      sendText("\x7f");
    });
    // Keep the on-screen keyboard open unless another field took the focus.
    listen(textarea, "blur", () => setTimeout(() => {
      if (closed) return;
      const active = document.activeElement;
      if (active && active !== textarea && ["INPUT", "TEXTAREA", "SELECT"].includes(active.tagName)) return;
      textarea.focus();
    }, 50));
    listen(host, "click", () => term.focus());

    let lastY = 0;
    let moved = 0;
    listen(host, "touchstart", (event) => { lastY = event.touches[0].clientY; moved = 0; }, { passive: true, capture: true });
    // Captured before xterm.js sees it, so its own touch scrolling does not run as well.
    listen(host, "touchmove", (event) => {
      const point = event.touches[0];
      moved += lastY - point.clientY;
      lastY = point.clientY;
      event.preventDefault();
      event.stopPropagation();
      while (Math.abs(moved) >= TOUCH_SCROLL_STEP) {
        const direction = moved > 0 ? 1 : -1;
        if (term.modes.mouseTrackingMode !== "none") sendWheel(direction, point.clientX, point.clientY);
        else if (term.buffer.active.type === "alternate") sendText(direction < 0 ? "\x1b[A" : "\x1b[B");
        else term.scrollLines(direction);
        moved -= direction * TOUCH_SCROLL_STEP;
      }
    }, { passive: false, capture: true });
  }

  async function open() {
    try {
      await loadLibrary();
    } catch (failure) {
      if (closed) return;
      setState("failed");
      say(failure.message, ["Try again", open]);
      return;
    }
    if (closed) return;
    const token = (name) => getComputedStyle(root).getPropertyValue(name).trim();
    const fontFamily = token("--font-terminal") || "monospace";
    const fontSize = touch ? FONT_PHONE : FONT_DESKTOP;
    await terminalFonts(fontFamily, fontSize);
    if (closed) return;
    const background = token("--color-bg");
    stage.style.backgroundColor = background;
    // The 16 ANSI colors come from the theme, in xterm.js's own order of names.
    const names = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    const palette = Object.fromEntries(names.flatMap((name, index) => [
      [name, token(`--color-ansi-${index}`)],
      [`bright${name[0].toUpperCase()}${name.slice(1)}`, token(`--color-ansi-${index + 8}`)],
    ]));
    term = new globalThis.Terminal({
      cursorBlink: true, fontSize, fontFamily,
      theme: { ...palette, background, foreground: token("--color-text"), cursor: token("--color-accent"), cursorAccent: background,
        selectionBackground: `${token("--color-accent")}55` },
      scrollback: SCROLLBACK, allowProposedApi: true, disableStdin: !canType,
    });
    fit = new globalThis.FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(host);
    silenceQueries(term);
    if (store.site.terminal.ligatures) joinLigatures(term);
    else host.dataset.ligatures = "off";
    // A font that arrived after the first measurement: set the family again so the cell is measured again.
    listen(document.fonts, "loadingdone", () => {
      term.options.fontFamily = `${fontFamily}, monospace`;
      term.options.fontFamily = fontFamily;
      layout();
    });
    for (const [name, value] of [["autocomplete", "off"], ["autocorrect", "off"], ["autocapitalize", "off"], ["spellcheck", "false"]]) {
      term.textarea.setAttribute(name, value);
    }
    term.textarea.setAttribute("aria-label", "Terminal input");
    if (touch && canType) touchInput();
    term.onData(sendText);
    term.onBinary((data) => send(Uint8Array.from(data, (character) => character.charCodeAt(0))));
    // One resize after the size settles: each one makes wsxd send a new replay.
    let resizing = 0;
    term.onResize(() => {
      clearTimeout(resizing);
      resizing = setTimeout(() => { if (!hostSize && !closed) sendControl({ type: "resize", rows: term.rows, cols: term.cols }); }, 80);
    });
    disposers.push(() => clearTimeout(resizing));
    const observer = new ResizeObserver(() => requestAnimationFrame(layout));
    observer.observe(stage);
    disposers.push(() => observer.disconnect());
    // main.js pins the page to the visual viewport; the grid follows the new height.
    if (window.visualViewport) listen(window.visualViewport, "resize", () => requestAnimationFrame(layout));
    // A phone that wakes up or regains its network reconnects at once.
    listen(window, "online", () => { if (state === "reconnecting") connect(); });
    listen(document, "visibilitychange", () => { if (!document.hidden && state === "reconnecting") connect(); });
    // ^ Opening the terminal is the person's action, so it takes the pane from a wsx
    // window. A reconnect never does, or two clients would take it back and forth.
    connect({ takeover: canType });
  }

  updateSizeToggle();
  open();

  return {
    close: () => {
      closed = true;
      generation += 1;
      clearTimeout(retryTimer);
      socket?.close();
      for (const dispose of disposers) dispose();
      // xterm.js starts a zero-delay timer when it opens; disposing before that timer
      // runs makes it throw. A terminal closed right after it opened is disposed one turn later.
      const opened = term;
      setTimeout(() => opened?.dispose(), 0);
      root.remove();
      sizeToggle.remove();
    },
  };
}
