// Mermaid diagrams in agent messages (docs/web-client.md "Diagrams").
// ^ The page's CSP has no 'unsafe-inline' and stays that way. Mermaid draws into the
// page to measure text, so while it draws, a `style` attribute it sets goes through
// the CSSOM, and its CSS is adopted as a constructed style sheet. The finished SVG is
// shown only as an image from a blob: URL: no script, link or request can come from it.
import { html, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";

// scripts/vendor-mermaid.sh checks this name against the version it writes.
const LIBRARY = "./mermaid-11.17.2.js";
const MAX_SOURCE_CHARS = 20_000;
const MAX_KEPT = 40;

let library = null;
let turn = Promise.resolve();
let serial = 0;
// Drawn diagrams by scheme and source, so a timeline refresh does not draw again.
const kept = new Map();

const token = (name) => getComputedStyle(document.documentElement).getPropertyValue(`--${name}`).trim();
const currentScheme = () => document.documentElement.dataset.scheme === "light" ? "light" : "dark";

// The diagram's colors are the theme's roles. Mermaid derives the rest from them.
function theme() {
  const [bg, surface, raised, text, muted, faint, attention] = ["bg", "surface", "raised", "text", "muted", "faint", "attention"].map((role) => token(`color-${role}`));
  return {
    darkMode: currentScheme() === "dark", fontFamily: token("font-body"), fontSize: token("font-size-md"),
    background: bg, primaryColor: raised, primaryTextColor: text, primaryBorderColor: faint,
    secondaryColor: surface, secondaryTextColor: text, secondaryBorderColor: faint,
    tertiaryColor: surface, tertiaryTextColor: text, tertiaryBorderColor: faint,
    lineColor: muted, textColor: text, mainBkg: raised, nodeBorder: faint, clusterBkg: surface, clusterBorder: faint,
    edgeLabelBackground: bg, titleColor: text,
    actorBkg: raised, actorBorder: faint, actorTextColor: text, actorLineColor: faint, signalColor: muted, signalTextColor: text,
    labelBoxBkgColor: surface, labelBoxBorderColor: faint, labelTextColor: text, loopTextColor: text,
    noteBkgColor: surface, noteTextColor: text, noteBorderColor: attention, activationBkgColor: raised, activationBorderColor: faint,
  };
}

// One drawing at a time: Mermaid's configuration and the two hooks below are global.
async function draw(source) {
  library ??= import(LIBRARY);
  await library;
  const mermaid = globalThis.mermaid;
  mermaid.initialize({
    startOnLoad: false, securityLevel: "strict", suppressErrorRendering: true, htmlLabels: false,
    flowchart: { htmlLabels: false }, theme: "base", themeVariables: theme(),
  });
  const setAttribute = Element.prototype.setAttribute;
  let sheet = null;
  globalThis.__wsxDiagramStyle = (css) => {
    sheet = new CSSStyleSheet();
    sheet.replaceSync(css);
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
  };
  Element.prototype.setAttribute = function (name, value) {
    if (name === "style") this.style.cssText = value;
    else setAttribute.call(this, name, value);
  };
  let svg;
  try {
    ({ svg } = await mermaid.render(`wsx-diagram-${++serial}`, source));
  } finally {
    Element.prototype.setAttribute = setAttribute;
    document.adoptedStyleSheets = document.adoptedStyleSheets.filter((other) => other !== sheet);
  }
  // The text is HTML-serialized SVG: make it a complete XML document with a real size.
  const box = /viewBox="[-\d.]+ [-\d.]+ ([\d.]+) ([\d.]+)"/.exec(svg);
  if (!box) throw new Error("no viewBox");
  const [width, height] = [Math.ceil(box[1]), Math.ceil(box[2])];
  svg = svg.replace("<metadata>", "<style>").replace("</metadata>", "</style>").replaceAll("&nbsp;", "&#160;")
    .replace(/^<svg [^>]*>/, (tag) => tag.replace(/ (width|height|style|xmlns:xlink)="[^"]*"/g, "")
      .replace("<svg ", `<svg xmlns:xlink="http://www.w3.org/1999/xlink" width="${width}" height="${height}" `));
  const url = URL.createObjectURL(new Blob([svg], { type: "image/svg+xml" }));
  const image = new Image();
  image.src = url;
  try {
    await image.decode();
  } catch (error) {
    URL.revokeObjectURL(url);
    throw error;
  }
  return { url, width, height };
}

function drawn(source, scheme) {
  const key = `${scheme}\n${source}`;
  if (!kept.has(key)) {
    const result = source.length > MAX_SOURCE_CHARS ? Promise.resolve({ failed: true })
      : (turn = turn.then(() => draw(source)).catch(() => ({ failed: true })));
    kept.set(key, result);
    if (kept.size > MAX_KEPT) {
      const [oldest, gone] = kept.entries().next().value;
      kept.delete(oldest);
      gone.then((entry) => entry.url && URL.revokeObjectURL(entry.url));
    }
  }
  return kept.get(key);
}

// The device's scheme choice, followed while a diagram is on the screen.
function useScheme() {
  const [scheme, setScheme] = useState(currentScheme());
  useEffect(() => {
    const observer = new MutationObserver(() => setScheme(currentScheme()));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-scheme"] });
    return () => observer.disconnect();
  }, []);
  return scheme;
}

const ZOOM_STEP = 1.5;
const MIN_SHOWN_SCALE = 0.75;

// Full screen: drag to move, pinch or the two buttons to zoom. The page itself never zooms.
function Viewer({ diagram, onClose }) {
  const dialog = useRef();
  const stage = useRef();
  const [zoom, setZoom] = useState(1);
  const pointers = useRef(new Map());
  const pinch = useRef(null);
  useEffect(() => { dialog.current.showModal(); }, []);
  // At zoom 1 the whole diagram fits the stage.
  const fit = () => Math.min(stage.current.clientWidth / diagram.width, stage.current.clientHeight / diagram.height, 1);
  const [base, setBase] = useState(null);
  useEffect(() => { setBase(fit()); }, []);
  const clamp = (value) => Math.min(8, Math.max(1, value));
  const spread = () => {
    const [a, b] = [...pointers.current.values()];
    return Math.hypot(a.x - b.x, a.y - b.y);
  };
  const down = (event) => {
    try { stage.current.setPointerCapture(event.pointerId); } catch { /* a pointer that already ended */ }
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.current.size === 2) pinch.current = { spread: spread(), zoom };
  };
  const move = (event) => {
    const last = pointers.current.get(event.pointerId);
    if (!last) return;
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.current.size === 2 && pinch.current) {
      setZoom(clamp(pinch.current.zoom * spread() / pinch.current.spread));
    } else if (pointers.current.size === 1) {
      stage.current.scrollLeft -= event.clientX - last.x;
      stage.current.scrollTop -= event.clientY - last.y;
    }
  };
  const up = (event) => {
    pointers.current.delete(event.pointerId);
    pinch.current = null;
  };
  const wheel = (event) => {
    if (!event.ctrlKey) return;
    event.preventDefault();
    setZoom((value) => clamp(value * (event.deltaY < 0 ? 1.1 : 1 / 1.1)));
  };
  const scale = (base ?? 1) * zoom;
  return html`
    <dialog class="viewer" ref=${dialog} aria-label="Diagram" onClose=${onClose}>
      <div class="viewer-bar">
        <button class="button" type="button" aria-label="Zoom out" disabled=${zoom <= 1} onClick=${() => setZoom((value) => clamp(value / ZOOM_STEP))}>−</button>
        <span class="time" aria-live="polite">${Math.round(zoom * 100)}%</span>
        <button class="button" type="button" aria-label="Zoom in" disabled=${zoom >= 8} onClick=${() => setZoom((value) => clamp(value * ZOOM_STEP))}>+</button>
        <button class="button" type="button" autofocus onClick=${() => dialog.current.close()}>Close</button>
      </div>
      <div class="viewer-stage" ref=${stage} onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerCancel=${up} onWheel=${wheel}>
        ${base !== null && html`<img src=${diagram.url} alt="Diagram" draggable=${false}
          width=${Math.round(diagram.width * scale)} height=${Math.round(diagram.height * scale)} />`}
      </div>
    </dialog>`;
}

// `fallback` is the diagram's source as a code block, shown when it cannot be drawn.
export function Diagram({ source, fallback }) {
  const scheme = useScheme();
  const [diagram, setDiagram] = useState(null);
  const [open, setOpen] = useState(false);
  useEffect(() => {
    let current = true;
    setDiagram(null);
    drawn(source, scheme).then((result) => { if (current) setDiagram(result); });
    return () => { current = false; };
  }, [source, scheme]);
  // A diagram a little wider than the column is shown smaller; a much wider one keeps its
  // size, so its text stays readable, and scrolls sideways.
  const frame = useRef();
  // Safari does not focus a tapped button, so the way back is kept here.
  const opener = useRef();
  const [fits, setFits] = useState(false);
  useLayoutEffect(() => {
    if (frame.current && diagram?.width) setFits(diagram.width * MIN_SHOWN_SCALE <= frame.current.clientWidth);
  }, [diagram]);
  if (!diagram) return html`<p class="note" role="status">Drawing the diagram…</p>`;
  if (diagram.failed) return html`<div class="diagram-failed">${fallback}<p class="note">Diagram could not be drawn</p></div>`;
  return html`
    <div class="diagram" ref=${frame} data-fits=${fits ? "" : null}>
      <button class="diagram-open" type="button" ref=${opener} aria-label="Open the diagram full screen" onClick=${() => setOpen(true)}>
        <img src=${diagram.url} width=${diagram.width} height=${diagram.height} alt="Diagram" />
      </button>
      ${open && html`<${Viewer} diagram=${diagram} onClose=${() => { setOpen(false); opener.current.focus(); }} />`}
    </div>`;
}
