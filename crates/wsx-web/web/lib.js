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
