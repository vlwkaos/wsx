// Images full screen over the page (docs/web-client.md "Image viewer"): a To check capture, a
// chat image, a drawn diagram; a To check video plays here with its own controls and no zoom.
// Pinch, the buttons, a double tap or ctrl+wheel zoom; a drag moves a zoomed image; a swipe or
// an arrow key goes to the other images of the same set. ×, Escape, a tap beside the image (not
// zoomed) and Back close it: it has its own history entry, so the phone's back gesture closes it
// and the page under it keeps its place.
import { html, Icon, render, useEffect, useLayoutEffect, useRef, useState } from "./lib.js";

const ZOOM_STEP = 1.5;
const MAX_ZOOM = 8;
const TAP_ZOOM = 2.5;
const TAP_MS = 300;
const TAP_PX = 10;
const SWIPE_PX = 50;

// Show `images` ([{ src, alt, width?, height?, video?, poster?, download? }]) from `index`; the focus goes
// back to `opener`.
// `dark` puts the images on the dark scheme whatever the page's is (a diagram keeps the page's:
// it is drawn in it).
export function openViewer(images, index, opener, { dark = true } = {}) {
  const host = document.body.appendChild(document.createElement("div"));
  const closed = () => {
    render(null, host);
    host.remove();
    opener?.focus({ preventScroll: true });
  };
  render(html`<${Viewer} images=${images} start=${index} dark=${dark} onClosed=${closed} />`, host);
}

// A plain tap on a link marked `data-view` opens its image here, with the other `data-view`
// links of its closest `data-gallery`; with a modifier key the link opens as a link does.
export function viewImageLinks() {
  addEventListener("click", (event) => {
    const link = event.target.closest?.("a[data-view]");
    if (!link || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    const group = link.closest("[data-gallery]");
    const links = group ? [...group.querySelectorAll("a[data-view]")] : [link];
    const images = links.map((each) => ({ src: each.href, alt: each.querySelector("img")?.alt || each.getAttribute("aria-label") || "",
      video: each.hasAttribute("data-video"), poster: each.dataset.poster, download: each.dataset.download }));
    openViewer(images, links.indexOf(link), link);
  });
}

function Viewer({ images, start, dark, onClosed }) {
  const dialog = useRef();
  const stage = useRef();
  const picture = useRef();
  const [at, setAt] = useState(start);
  const image = images[at];
  // ^ An image's size: the opener's when it knows it (a diagram), else measured when it loads.
  const [measured, setMeasured] = useState(null);
  const natural = image.width ? image : measured;
  const [room, setRoom] = useState(null);
  const [zoom, setZoom] = useState(1);
  const pointers = useRef(new Map());
  const pinch = useRef(null);
  const swipe = useRef(null);
  const lastTap = useRef(null);
  const beside = useRef(false);
  const done = useRef(false);
  const [broken, setBroken] = useState(false);
  // ^ The image point under a zoom's center, kept there after the zoom (see the layout effect).
  const anchor = useRef(null);

  const finish = () => {
    if (done.current) return;
    done.current = true;
    if (dialog.current.open) dialog.current.close();
    onClosed();
  };
  // Back leaves the viewer's entry: the viewer closes. × and Escape go Back the same way.
  const close = () => { if (history.state?.viewer) history.back(); else finish(); };
  useEffect(() => {
    dialog.current.showModal();
    history.pushState({ ...history.state, viewer: true }, "");
    const popped = () => { if (!history.state?.viewer) finish(); };
    addEventListener("popstate", popped);
    const observer = new ResizeObserver(() => setRoom({ width: stage.current.clientWidth, height: stage.current.clientHeight }));
    observer.observe(stage.current);
    return () => {
      removeEventListener("popstate", popped);
      observer.disconnect();
    };
  }, []);

  const go = (step) => {
    const next = at + step;
    if (next < 0 || next >= images.length) return;
    setAt(next);
    setZoom(1);
    setBroken(false);
    setMeasured(null);
  };
  // At zoom 1 the whole image fits the screen.
  const fit = natural && room ? Math.min(room.width / natural.width, room.height / natural.height) : null;
  const zoomTo = (value, x, y) => {
    const next = Math.min(MAX_ZOOM, Math.max(1, value));
    const box = picture.current?.getBoundingClientRect();
    if (box?.width) anchor.current = { x, y, fx: (x - box.left) / box.width, fy: (y - box.top) / box.height };
    setZoom(next);
  };
  const zoomCenter = (value) => {
    const box = stage.current.getBoundingClientRect();
    zoomTo(value, box.left + box.width / 2, box.top + box.height / 2);
  };
  useLayoutEffect(() => {
    const point = anchor.current;
    anchor.current = null;
    if (!point || !picture.current) return;
    const box = picture.current.getBoundingClientRect();
    stage.current.scrollLeft += box.left + point.fx * box.width - point.x;
    stage.current.scrollTop += box.top + point.fy * box.height - point.y;
  }, [zoom]);

  const spread = () => {
    const [a, b] = [...pointers.current.values()];
    return { distance: Math.hypot(a.x - b.x, a.y - b.y), x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
  };
  const slide = (offset) => {
    if (!picture.current) return;
    picture.current.style.translate = offset ? `${offset}px 0` : "";
    picture.current.toggleAttribute("data-swiping", Boolean(offset));
  };
  const down = (event) => {
    // ^ The video's own controls get its pointers: no capture, swipe or tap zoom on it.
    if (image.video && event.target === picture.current) return;
    try { stage.current.setPointerCapture(event.pointerId); } catch { /* a pointer that already ended */ }
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.current.size === 2) {
      pinch.current = { distance: spread().distance, zoom };
      swipe.current = null;
      slide(0);
    } else if (pointers.current.size === 1) {
      swipe.current = { x: event.clientX, y: event.clientY, moved: false, onImage: event.target === picture.current };
    }
  };
  const move = (event) => {
    const last = pointers.current.get(event.pointerId);
    if (!last) return;
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.current.size === 2 && pinch.current) {
      const now = spread();
      zoomTo(pinch.current.zoom * now.distance / pinch.current.distance, now.x, now.y);
    } else if (pointers.current.size === 1 && swipe.current) {
      const dx = event.clientX - swipe.current.x;
      if (Math.hypot(dx, event.clientY - swipe.current.y) > TAP_PX) swipe.current.moved = true;
      if (zoom > 1) {
        stage.current.scrollLeft -= event.clientX - last.x;
        stage.current.scrollTop -= event.clientY - last.y;
      } else if (images.length > 1 && swipe.current.moved) {
        slide(dx);
      }
    }
  };
  const up = (event) => {
    if (!pointers.current.delete(event.pointerId)) return;
    const gesture = swipe.current;
    swipe.current = null;
    if (pinch.current) {
      if (!pointers.current.size) pinch.current = null;
      return;
    }
    if (!gesture || event.type === "pointercancel") return slide(0);
    const dx = event.clientX - gesture.x;
    slide(0);
    if (gesture.moved) {
      if (zoom === 1 && Math.abs(dx) > SWIPE_PX) go(dx < 0 ? 1 : -1);
      return;
    }
    // A tap: twice on the image zooms in there (or back out); once beside it closes, on the
    // click that follows, so that click cannot reach the page under the viewer.
    if (!gesture.onImage) {
      beside.current = zoom === 1;
      return;
    }
    const before = lastTap.current;
    const now = { at: event.timeStamp, x: event.clientX, y: event.clientY };
    if (before && now.at - before.at < TAP_MS && Math.hypot(now.x - before.x, now.y - before.y) < TAP_PX * 3) {
      lastTap.current = null;
      zoomTo(zoom > 1 ? 1 : TAP_ZOOM, now.x, now.y);
    } else {
      lastTap.current = now;
    }
  };
  const wheel = (event) => {
    if (!event.ctrlKey || image.video) return;
    event.preventDefault();
    zoomTo(zoom * (event.deltaY < 0 ? 1.1 : 1 / 1.1), event.clientX, event.clientY);
  };
  const keys = (event) => {
    if (event.key === "ArrowLeft") go(-1);
    else if (event.key === "ArrowRight") go(1);
    else return;
    event.preventDefault();
  };
  // ^ iPhone: the share sheet can save to Photos, but its files must be read first, which may end
  // the tap's permission; then the attachment URL is opened, which saves to Files. Elsewhere
  // the link's own `download` runs.
  const save = async (event) => {
    if (!matchMedia("(pointer: coarse)").matches || !navigator.canShare || !navigator.share) return;
    event.preventDefault();
    try {
      const response = await fetch(image.download);
      if (!response.ok) throw new Error("not readable");
      const name = /filename="([^"]+)"/.exec(response.headers.get("content-disposition") ?? "")?.[1] ?? "capture";
      const blob = await response.blob();
      const file = new File([blob], name, { type: blob.type });
      if (!navigator.canShare({ files: [file] })) throw new Error("cannot share");
      await navigator.share({ files: [file] });
    } catch (error) {
      if (error?.name !== "AbortError") location.href = image.download;
    }
  };
  const cancel = (event) => { event.preventDefault(); close(); };
  const clicked = () => {
    if (beside.current) close();
    beside.current = false;
  };

  const many = images.length > 1;
  const scale = (fit ?? 1) * zoom;
  return html`
    <dialog class="viewer" ref=${dialog} data-scheme=${dark ? "dark" : undefined}
      aria-label=${image.alt || (image.video ? "Video" : "Image")} onCancel=${cancel} onClose=${() => { if (!done.current) close(); }} onKeyDown=${keys}>
      <div class="viewer-bar">
        ${many && html`
          <button class="icon-button" type="button" aria-label="Previous image" disabled=${at === 0} onClick=${() => go(-1)}><${Icon} name="back" /></button>
          <span class="time" aria-live="polite">${at + 1} of ${images.length}</span>
          <button class="icon-button" type="button" aria-label="Next image" disabled=${at === images.length - 1} onClick=${() => go(1)}><${Icon} name="chevron" /></button>`}
        <span class="viewer-zoom">${!image.video && html`
          <button class="button" type="button" aria-label="Zoom out" disabled=${zoom <= 1} onClick=${() => zoomCenter(zoom / ZOOM_STEP)}>−</button>
          <span class="time" aria-live="polite">${Math.round(zoom * 100)}%</span>
          <button class="button" type="button" aria-label="Zoom in" disabled=${zoom >= MAX_ZOOM} onClick=${() => zoomCenter(zoom * ZOOM_STEP)}>+</button>`}
        </span>
        ${image.download && html`<a class="icon-button" href=${image.download} download aria-label="Download" title="Download" onClick=${save}><${Icon} name="down" /></a>`}
        <button class="icon-button" type="button" aria-label="Close" autofocus onClick=${close}><${Icon} name="fail" /></button>
      </div>
      <div class="viewer-stage" ref=${stage} data-video=${image.video || undefined} onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerCancel=${up} onWheel=${wheel} onClick=${clicked}>
        ${broken && html`<p class="note viewer-note" role="alert">The ${image.video ? "video" : "image"} could not be read.</p>`}
        ${image.video ? html`
        <video key=${image.src} ref=${picture} src=${image.src} poster=${image.poster} controls playsinline preload="metadata"
          aria-label=${image.alt} data-loading=${fit === null || broken || undefined}
          width=${natural && Math.round(natural.width * scale)} height=${natural && Math.round(natural.height * scale)}
          onLoadedMetadata=${(event) => setMeasured({ width: event.target.videoWidth, height: event.target.videoHeight })}
          onError=${() => setBroken(true)}></video>` : html`
        <img key=${image.src} ref=${picture} src=${image.src} alt=${image.alt} draggable=${false} data-loading=${fit === null || broken || undefined}
          width=${natural && Math.round(natural.width * scale)} height=${natural && Math.round(natural.height * scale)}
          onLoad=${(event) => setMeasured({ width: event.target.naturalWidth, height: event.target.naturalHeight })}
          onError=${() => setBroken(true)} />`}
      </div>
    </dialog>`;
}
