// Phone layout audit: evaluate in the rendered page at a phone viewport (for example 390x844,
// and 390x420 for the keyboard-up case); returns JSON measurements, not verdicts.
// Rules and thresholds: reference/MOBILE.md of the ui-view-standards skill; this is that skill's
// scripts/mobile-audit.js, kept here so the web checks run where the skill is not installed.
// Playwright: await page.evaluate(source)
// Optional before evaluating: globalThis.MOBILE_AUDIT_GUTTER = 16 (the product's side inset token)
(() => {
  const LIMIT = 8, TARGET = 44, INPUT_FONT = 16, MIN_TEXT = 12;
  const gutter = globalThis.MOBILE_AUDIT_GUTTER ?? 16;
  const vv = window.visualViewport, vw = innerWidth, vh = vv ? vv.height : innerHeight;
  const all = [...document.querySelectorAll('body *')];
  const styleOf = new Map(all.map((el) => [el, getComputedStyle(el)]));
  const visible = all.filter((el) => {
    const s = styleOf.get(el), r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < vh && s.visibility !== 'hidden' && s.display !== 'none' && +s.opacity > 0;
  });
  const name = (el) => {
    let n = el.tagName.toLowerCase();
    if (el.id) return n + '#' + el.id;
    const cls = [...el.classList].slice(0, 2).join('.');
    return cls ? n + '.' + cls : n;
  };
  const label = (el) => (el.getAttribute('aria-label') || el.innerText || el.value || '').replace(/\s+/g, ' ').trim().slice(0, 40);
  const cap = (items) => ({ count: items.length, items: items.slice(0, LIMIT), more: Math.max(0, items.length - LIMIT) });
  const scrolls = (el) => /(auto|scroll)/.test(styleOf.get(el).overflowY) && el.scrollHeight > el.clientHeight + 1;
  const scrollsX = (el) => /(auto|scroll)/.test(styleOf.get(el).overflowX) && el.scrollWidth > el.clientWidth + 1;

  // Content share: the largest vertically scrolling region's visible height. The rest of the
  // height is chrome (top row, tab bar, composer, key rows) that the reader cannot scroll away.
  // Menus and lists over the content (a `/` list, a select popup) are not the content.
  const regions = visible.filter((el) => /(auto|scroll)/.test(styleOf.get(el).overflowY) && !el.closest('[role=listbox], [role=menu], dialog'))
    .map((el) => { const r = el.getBoundingClientRect(); return { el, h: Math.min(r.bottom, vh) - Math.max(r.top, 0), w: r.width }; })
    .filter((x) => x.w >= vw * 0.6).sort((a, b) => b.h - a.h);
  const main = regions[0];
  const page = document.scrollingElement.scrollHeight > vh + 1;
  const contentShare = main ? +(main.h / vh).toFixed(3) : page ? 1 : null;

  // Touch targets: interactive elements smaller than 44x44 in either axis (Apple HIG).
  const controls = visible.filter((el) => el.matches('a[href], button, input:not([type=hidden]), select, textarea, summary, [role=button], [role=tab], [role=link], [tabindex]:not([tabindex="-1"])'));
  // A checkbox or radio inside its label is as large as the label.
  const small = controls.map((el) => ({ el, r: (el.matches('input[type=checkbox], input[type=radio]') && el.closest('label') || el).getBoundingClientRect() }))
    .filter(({ el, r }) => (r.width < TARGET || r.height < TARGET) && !(el.matches('a') && el.closest('p, li, td')))
    .map(({ el, r }) => ({ el: name(el), label: label(el), size: [Math.round(r.width), Math.round(r.height)] }));

  // iOS Safari zooms the page when a focused field's font is below 16 px.
  const fields = visible.filter((el) => el.matches('input:not([type=hidden]):not([type=checkbox]):not([type=radio]):not([type=file]), textarea, select, [contenteditable=""], [contenteditable=true]'));
  const zoomingFields = fields.filter((el) => parseFloat(styleOf.get(el).fontSize) < INPUT_FONT)
    .map((el) => ({ el: name(el), fontSize: styleOf.get(el).fontSize }));

  // iOS form bar: with a field focused, every other focusable field enables previous/next.
  const focused = document.activeElement;
  const otherFields = focused && focused.matches?.('input, textarea, select, [contenteditable]')
    ? [...document.querySelectorAll('input:not([type=hidden]), textarea, select, [contenteditable=""], [contenteditable=true]')]
      .filter((el) => el !== focused && !el.disabled && el.tabIndex >= 0).map(name)
    : null;

  // Text: too small, or outside the side gutter (unless inside its own horizontal scroller).
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  const tiny = [], edge = [];
  for (let n; (n = walker.nextNode());) {
    const p = n.parentElement, t = n.nodeValue.trim();
    if (!t || !p || !visible.includes(p) || p.closest('script, style, noscript')) continue;
    if (parseFloat(styleOf.get(p).fontSize) < MIN_TEXT) tiny.push({ el: name(p), text: t.slice(0, 40), fontSize: styleOf.get(p).fontSize });
    const range = document.createRange(); range.selectNodeContents(n);
    // Text cut off by its box (an ellipsis) is measured as far as the box shows it.
    const full = range.getBoundingClientRect(), box = p.getBoundingClientRect();
    const clipped = /(hidden|clip)/.test(styleOf.get(p).overflowX);
    const r = clipped ? { width: full.width, left: Math.max(full.left, box.left), right: Math.min(full.right, box.right) } : full;
    let scroller = p; while (scroller && scroller !== document.body && !scrollsX(scroller)) scroller = scroller.parentElement;
    if (r.width && (scroller === document.body || !scroller) && (r.left < gutter - 1 || r.right > vw - gutter + 1)) edge.push({ el: name(p), text: t.slice(0, 40), left: Math.round(r.left), right: Math.round(vw - r.right) });
  }

  return {
    viewport: [vw, Math.round(vh)], layoutHeight: innerHeight, url: location.href,
    contentShare, contentRegion: main ? name(main.el) : page ? 'page' : null, regionScrolls: main ? scrolls(main.el) : page,
    smallTargets: cap(small),
    zoomingFields: cap(zoomingFields),
    focusedField: focused && focused !== document.body ? name(focused) : null,
    otherFocusableFields: otherFields && cap(otherFields),
    tinyText: cap(tiny),
    outsideGutter: cap(edge),
    horizontalOverflow: document.documentElement.scrollWidth > vw,
  };
})()
