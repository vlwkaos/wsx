// vendored: ui-view-standards skill scripts/design-lint.js (dotfiles #28), sha256 5cb8e45a34c5c63eafed676bd82741fb6003874924332f106417acf06b4479ba, copied 2026-11-07.
// vendored: the rest of this file is that script unchanged; web-cdp.mjs designLint() prints a line when the installed copy differs.
// Design lint: evaluate in the rendered page; returns findings at two levels, error and warn.
// Rules, sources, config and output: ../reference/LINT.md. Playwright: await page.evaluate(fs.readFileSync(path, 'utf8'))
// Optional before evaluating: globalThis.DESIGN_LINT = { touch, gutter, primary, keep: [{ rule, selector, reason }] }
(() => {
  // ^ id: [level, source, fix]. reference/LINT.md mirrors id, level and source and owns the thresholds; tests/test-design-lint.sh compares them.
  const RULES = {
    'gap.interactive': ['error', 'reference/LAYOUT.md:24', 'Put a spacing token between the control and its neighbour, or give the control a visible box'],
    'overlap': ['error', 'reference/LAYOUT.md:25', 'Reflow so text and controls do not draw over each other'],
    'text.clipped': ['error', 'reference/LAYOUT.md:34', 'Wrap the text, or truncate it with an ellipsis and the full value on hover and focus'],
    'target.size': ['error', 'reference/LAYOUT.md:35', 'Grow the hit area with padding or min-width and min-height'],
    'contrast': ['error', 'reference/VISUAL.md:40', 'Use a text token with AA contrast on this background'],
    'overflow.page': ['error', 'reference/MOBILE.md:14', 'Let the element wrap, or scroll it inside its own block'],
    'color.raw': ['warn', 'SKILL.md:29', 'Use a colour token'],
    'spacing.scale': ['warn', 'SKILL.md:29', 'Use a spacing token'],
    'slop.palette': ['warn', 'reference/SLOP.md:10', 'Neutral base, one accent and semantic status roles'],
    'slop.motion': ['warn', 'reference/SLOP.md:11', 'Show constant state as static text; animate only real work'],
    'slop.nested-cards': ['warn', 'reference/SLOP.md:12', 'Use rows, dividers or whitespace inside the outer card'],
    'slop.emoji': ['warn', 'reference/SLOP.md:13', 'Remove it, or use an icon with an accessible name'],
    'slop.filler': ['warn', 'reference/SLOP.md:16', 'Delete it, or move it to an about page'],
    'slop.glass': ['warn', 'reference/SLOP.md:17', 'Use an opaque token surface'],
    'slop.marketing': ['warn', 'reference/SLOP.md:18', 'Open on the task; name the actual effect'],
    'slop.primary': ['warn', 'reference/SLOP.md:20', 'Keep one filled primary; make the others secondary'],
    'primary.offscreen': ['warn', 'reference/SLOP.md:20', 'Move the primary action into the first viewport'],
    'layout.dead-space': ['warn', 'reference/LAYOUT.md:14', 'Put the controls in one row above or below the content'],
    'layout.wide-gap': ['warn', 'reference/LAYOUT.md:23', 'Keep the action next to its object; group the toolbar'],
    'mobile.content-share': ['warn', 'reference/MOBILE.md:11', 'Shrink or merge chrome rows'],
    'mobile.gutter': ['warn', 'reference/MOBILE.md:14', 'Use the side inset token; scroll wide blocks inside themselves'],
    'input.zoom': ['warn', 'reference/MOBILE.md:25', 'Set the field font to 16 px or more'],
    'text.tiny': ['warn', 'reference/MOBILE.md:26', 'Use a type token of 12 px or more'],
  };

  const cfg = globalThis.DESIGN_LINT || {};
  const keep = cfg.keep || [];
  if (!Array.isArray(keep)) throw new Error('design-lint: keep must be an array');
  for (const k of keep) {
    if (!RULES[k?.rule] || typeof k.selector !== 'string' || !String(k.reason ?? '').trim()) throw new Error('design-lint: keep entry needs a known rule, a selector and a reason: ' + JSON.stringify(k));
    document.querySelector(k.selector); // ^ throws on an invalid selector
  }
  const touch = cfg.touch ?? matchMedia('(pointer: coarse)').matches;
  const gutter = cfg.gutter ?? 16;
  const vw = innerWidth, vh = window.visualViewport ? visualViewport.height : innerHeight;
  const doc = document.documentElement;

  const all = [...document.querySelectorAll('body *')];
  const styleOf = new Map(all.map((el) => [el, getComputedStyle(el)]));
  const visible = all.filter((el) => {
    const s = styleOf.get(el), r = el.getBoundingClientRect();
    return r.width > 1 && r.height > 1 && s.visibility !== 'hidden' && +s.opacity > 0;
  });
  const shown = new Set(visible);

  const sel = (el) => {
    const parts = [];
    for (let e = el; e && e !== document.body && e !== doc && parts.length < 4; e = e.parentElement) {
      if (e.id) { parts.unshift('#' + CSS.escape(e.id)); break; }
      let p = e.tagName.toLowerCase() + [...e.classList].slice(0, 2).map((c) => '.' + CSS.escape(c)).join('');
      const peers = [...(e.parentElement?.children || [])].filter((x) => x.tagName === e.tagName);
      if (peers.length > 1) p += `:nth-of-type(${peers.indexOf(e) + 1})`;
      parts.unshift(p);
    }
    return parts.join(' > ') || el.tagName.toLowerCase();
  };
  const snip = (t) => String(t).replace(/\s+/g, ' ').trim().slice(0, 80);
  const textOf = (el) => el.getAttribute('aria-label') || el.labels?.[0]?.innerText || el.innerText || el.value || '';
  const findings = [], seen = {};
  // ^ One finding per rule and key (the element, or a value for rules grouped by value); r defaults to the element's box.
  const add = (rule, el, text, { r = el.getBoundingClientRect(), key = el } = {}) => {
    const done = (seen[rule] ??= new Set());
    if (done.has(key)) return;
    done.add(key);
    const [level, , fix] = RULES[rule];
    const k = keep.find((x) => x.rule === rule && el.matches(x.selector));
    findings.push({ rule, level, element: sel(el), text: snip(text), rect: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) }, fix, ...(k ? { kept: k.reason } : {}) });
  };

  // Colours: a 1x1 canvas turns any computed colour (rgb, oklch, color()) into sRGB bytes.
  const ctx = Object.assign(document.createElement('canvas'), { width: 1, height: 1 }).getContext('2d', { willReadFrequently: true });
  const rgbaCache = new Map();
  const rgba = (c) => {
    if (!rgbaCache.has(c)) {
      ctx.clearRect(0, 0, 1, 1); ctx.fillStyle = '#000'; ctx.fillStyle = c; ctx.fillRect(0, 0, 1, 1);
      const [r, g, b, a] = ctx.getImageData(0, 0, 1, 1).data;
      rgbaCache.set(c, [r, g, b, a / 255]);
    }
    return rgbaCache.get(c);
  };
  const hsl = ([r, g, b]) => {
    r /= 255; g /= 255; b /= 255;
    const max = Math.max(r, g, b), min = Math.min(r, g, b), l = (max + min) / 2, d = max - min;
    if (!d) return [0, 0, l];
    const s = d / (1 - Math.abs(2 * l - 1));
    const h = max === r ? ((g - b) / d) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
    return [(h * 60 + 360) % 360, s, l];
  };
  const saturatedHue = (c) => {
    const v = rgba(c); if (v[3] <= 0.05) return null;
    const [h, s, l] = hsl(v);
    return s > 0.3 && l > 0.15 && l < 0.85 ? h : null;
  };
  // ^ A hidden element under the body resolves token values and system colours; it never stays in the page.
  const withProbe = (fn) => {
    const probe = document.body.appendChild(document.createElement('div'));
    probe.style.cssText = 'position:absolute;visibility:hidden';
    try { return fn(probe); } finally { probe.remove(); }
  };
  const SIDES = ['Top', 'Right', 'Bottom', 'Left'];
  const paints = (s) => rgba(s.backgroundColor)[3] > 0.05 || s.backgroundImage !== 'none' || s.boxShadow !== 'none'
    || SIDES.some((d) => parseFloat(s[`border${d}Width`]) > 0 && rgba(s[`border${d}Color`])[3] > 0.05);

  // ^ The area an element's content shows in: its own box and every ancestor box that clips or scrolls overflow.
  const ALL = { L: -Infinity, T: -Infinity, R: Infinity, B: Infinity };
  const padCache = new Map(), clipCache = new Map();
  const padBox = (e) => {
    if (!padCache.has(e)) { const b = e.getBoundingClientRect(), L = b.left + e.clientLeft, T = b.top + e.clientTop; padCache.set(e, { L, T, R: L + e.clientWidth, B: T + e.clientHeight }); }
    return padCache.get(e);
  };
  const narrow = (c, e, x, y) => { const b = padBox(e); return { L: x ? Math.max(c.L, b.L) : c.L, R: x ? Math.min(c.R, b.R) : c.R, T: y ? Math.max(c.T, b.T) : c.T, B: y ? Math.min(c.B, b.B) : c.B }; };
  const clipOf = (el) => {
    const s = el && styleOf.get(el);
    if (!s) return ALL;
    if (!clipCache.has(el)) {
      const c = s.position === 'fixed' ? ALL : clipOf(el.parentElement), x = s.overflowX !== 'visible', y = s.overflowY !== 'visible';
      clipCache.set(el, x || y ? narrow(c, el, x, y) : c);
    }
    return clipCache.get(el);
  };
  const cut = (r, c) => {
    const L = Math.max(r.left, c.L), R = Math.min(r.right, c.R), T = Math.max(r.top, c.T), B = Math.min(r.bottom, c.B);
    return R - L > 1 && B - T > 1 ? new DOMRect(L, T, R - L, B - T) : null;
  };
  const union = (a, b) => { const L = Math.min(a.left, b.left), T = Math.min(a.top, b.top); return new DOMRect(L, T, Math.max(a.right, b.right) - L, Math.max(a.bottom, b.bottom) - T); };
  const contentRect = (node) => { const range = document.createRange(); range.selectNodeContents(node); return range.getBoundingClientRect(); };

  // Text nodes are scanned once and shared by every text rule. raw: the glyphs without edge spaces; rects: the part a reader sees.
  // lead and trail: a rendered (not collapsed) space before or after the text, which separates it from a neighbour.
  const texts = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let n; (n = walker.nextNode());) {
    const p = n.parentElement, v = n.nodeValue, start = v.search(/\S/), end = v.trimEnd().length;
    if (start < 0 || !shown.has(p) || p.closest('script, style, noscript')) continue;
    const span = (a, b) => { const range = document.createRange(); range.setStart(n, a); range.setEnd(n, b); return range; };
    const raw = [...span(start, end).getClientRects()].filter((r) => r.width > 0 && r.height > 0);
    const rects = raw.map((r) => cut(r, clipOf(p))).filter(Boolean);
    const spaced = (a, b) => a < b && span(a, b).getBoundingClientRect().width > 1;
    if (rects.length) texts.push({ p, t: v.trim(), raw, rects, lead: spaced(0, start), trail: spaced(end, v.length) });
  }
  const isCode = (el) => !!el.closest('code, pre, kbd, samp, textarea, input');
  const textHits = (rule, re, skipCode = true) => {
    for (const { p, t, rects } of texts) if ((!skipCode || !isCode(p)) && re.test(t)) add(rule, p, t, { r: rects[0] });
  };

  // Controls. A link among prose text is text; controls clipped out of sight (a closed panel, a scrolled list) are skipped.
  const CONTROL = 'a[href], button, input:not([type=hidden]), select, textarea, summary, [role=button], [role=link], [role=tab], [role=checkbox], [role=switch], [role=menuitem], [tabindex]:not([tabindex="-1"])';
  const proseCache = new Map();
  const proseLink = (el) => {
    if (!el.matches('a') || styleOf.get(el).display !== 'inline') return false;
    let b = el.parentElement;
    while (b !== document.body && styleOf.get(b).display === 'inline') b = b.parentElement;
    if (!proseCache.has(b)) proseCache.set(b, texts.some(({ p }) => b.contains(p) && !p.closest('a, button, [role=button], [role=link]')));
    return proseCache.get(b);
  };
  // ^ What a reader sees of a control: its painted box, or its content when nothing is painted.
  const boxOf = (el) => {
    if (!paints(styleOf.get(el))) { const b = contentRect(el); if (b.width && b.height) return b; }
    return el.getBoundingClientRect();
  };
  const seenBox = new Map();
  for (const el of visible) {
    const r = el.matches(CONTROL) && !proseLink(el) && cut(boxOf(el), clipOf(el.parentElement));
    if (r) seenBox.set(el, r);
  }
  const controls = [...seenBox.keys()];
  const ownControl = (el) => { for (let e = el; e && e !== document.body; e = e.parentElement) if (seenBox.has(e)) return e; return null; };

  // gap.interactive and overlap: text lines and control boxes, swept by top edge.
  const LAYER = 'dialog, [role=dialog], [role=menu], [role=listbox], [role=tooltip], [popover]';
  const DISABLED = ':disabled, [aria-disabled=true]';
  const layerCache = new Map();
  const layerOf = (el) => {
    if (!layerCache.has(el)) {
      const s = styleOf.get(el);
      layerCache.set(el, !s ? null : el.matches(LAYER) || /fixed|sticky/.test(s.position) ? el : layerOf(el.parentElement));
    }
    return layerCache.get(el);
  };
  const group = (el) => el.closest('[role=group], [role=tablist], [role=radiogroup]');
  const items = [
    ...texts.filter(({ p }) => !ownControl(p)).flatMap(({ p, t, rects, lead, trail }) => rects.map((r) => ({ el: p, r, t, lead, trail, control: false }))),
    ...controls.map((el) => ({ el, r: seenBox.get(el), t: textOf(el), control: true })),
  ].sort((a, b) => a.r.top - b.r.top);
  for (let i = 0; i < items.length; i++) {
    for (let j = i + 1; j < items.length && items[j].r.top < items[i].r.bottom; j++) {
      const a = items[i], b = items[j];
      if (a.el === b.el || (a.control && b.control && (a.el.contains(b.el) || b.el.contains(a.el)))) continue;
      const dy = Math.min(a.r.bottom, b.r.bottom) - Math.max(a.r.top, b.r.top), minH = Math.min(a.r.height, b.r.height);
      const gap = Math.max(b.r.left - a.r.right, a.r.left - b.r.right);
      const [c, o] = a.control ? [a, b] : [b, a];
      if (c.control && dy > minH / 2 && gap >= -2 && gap < 4) {
        // ^ Not glued: one segmented control, a field and its own label, or text set off by a rendered space.
        const joined = o.control ? group(c.el) && group(c.el) === group(o.el)
          : [...(c.el.labels || [])].some((l) => l.contains(o.el)) || (o.r.left >= c.r.right - 2 ? o.lead : o.trail);
        if (!joined) add('gap.interactive', c.el, `${snip(c.t)} | ${snip(o.t)} (${Math.round(gap)} px)`, { r: c.r });
      } else if (-gap > 2 && dy > Math.max(2, minH / 3) && layerOf(a.el) === layerOf(b.el)) {
        add('overlap', a.el, `${snip(a.t)} | ${snip(b.t)}`, { r: a.r });
      }
    }
  }

  // text.clipped: text partly cut by an overflow-hidden box that shows no ellipsis. Each text walks up its clipping boxes;
  // ^ past a box, only what that box shows (after its ellipsis, clamp or scroll) can be cut by an outer box.
  for (const { p, t, raw } of texts) {
    let rs = raw;
    for (let e = p; e && e !== document.body && rs.length; e = e.parentElement) {
      const s = styleOf.get(e), x = s.overflowX !== 'visible', y = s.overflowY !== 'visible';
      if (!x && !y) continue;
      const b = padBox(e), cx = /hidden|clip/.test(s.overflowX) && s.textOverflow !== 'ellipsis', cy = /hidden|clip/.test(s.overflowY) && s.webkitLineClamp === 'none';
      const r = rs.find((r) => r.left < b.R && r.right > b.L && r.top < b.B && r.bottom > b.T
        && ((cx && (r.left < b.L - 1 || r.right > b.R + 1)) || (cy && Math.max(b.T - r.top, r.bottom - b.B) > r.height / 4)));
      if (r) { add('text.clipped', p, t, { r, key: e }); break; }
      rs = rs.map((r) => cut(r, narrow(ALL, e, x, y))).filter(Boolean);
    }
  }

  // target.size: the hit area is the box plus its content (an image in an inline link); a checkbox or radio adds its labels.
  // ^ Off touch, WCAG 2.5.8 spacing applies: a small target passes when a 24 px circle on its centre reaches no other target.
  const TARGET = touch ? 44 : 24;
  const hitOf = (el) => {
    const c = contentRect(el), r = c.width && c.height ? union(el.getBoundingClientRect(), c) : el.getBoundingClientRect();
    return el.matches('input[type=checkbox], input[type=radio]') ? [...el.labels].reduce((u, l) => union(u, l.getBoundingClientRect()), r) : r;
  };
  const hits = controls.filter((el) => !el.matches(DISABLED)).map((el) => ({ el, r: hitOf(el) }));
  const small = hits.filter(({ r }) => r.width < TARGET - 0.5 || r.height < TARGET - 0.5);
  const centre = (r) => [r.left + r.width / 2, r.top + r.height / 2];
  const reach = (x, y, r) => Math.hypot(Math.max(r.left - x, 0, x - r.right), Math.max(r.top - y, 0, y - r.bottom));
  for (const a of small) {
    const [x, y] = centre(a.r), apart = (o) => o === a || o.el.contains(a.el) || a.el.contains(o.el);
    const crowded = touch || hits.some((o) => !apart(o) && reach(x, y, o.r) < 12) || small.some((o) => !apart(o) && Math.hypot(x - centre(o.r)[0], y - centre(o.r)[1]) < 24);
    if (crowded) add('target.size', a.el, `${snip(textOf(a.el))} ${Math.round(a.r.width)}x${Math.round(a.r.height)} < ${TARGET}`, { r: a.r });
  }

  // contrast: text colour over the composited ancestor backgrounds; unknown under a background image.
  const lum = (v) => { const f = (c) => ((c /= 255) <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4); return 0.2126 * f(v[0]) + 0.7152 * f(v[1]) + 0.0722 * f(v[2]); };
  const over = (top, under) => under.map((c, i) => top[i] * top[3] + c * (1 - top[3]));
  const canvas = withProbe((probe) => { probe.style.backgroundColor = 'Canvas'; return rgba(getComputedStyle(probe).backgroundColor).slice(0, 3); });
  for (const { p, t, rects } of texts) {
    if (p.closest(DISABLED)) continue;
    const layers = []; let alpha = 1, image = false;
    for (let e = p; e; e = e.parentElement) {
      const es = styleOf.get(e) || getComputedStyle(e);
      if (es.backgroundImage !== 'none') { image = true; break; }
      const c = rgba(es.backgroundColor);
      if (c[3] > 0) layers.push(c);
      if (c[3] >= 1) break;
      alpha *= +es.opacity;
    }
    const s = styleOf.get(p), fg = rgba(s.webkitTextFillColor || s.color);
    if (image || !fg[3]) continue;
    const bg = layers.reverse().reduce((u, c) => over(c, u), canvas);
    const [l1, l2] = [lum(over([...fg.slice(0, 3), fg[3] * alpha], bg)), lum(bg)].sort((x, y) => y - x);
    const ratio = (l1 + 0.05) / (l2 + 0.05), size = parseFloat(s.fontSize);
    const need = size >= 24 || (size >= 18.66 && +s.fontWeight >= 700) ? 3 : 4.5;
    if (ratio < need - 0.005) add('contrast', p, `${t} (${ratio.toFixed(2)}:1 < ${need}:1)`, { r: rects[0] });
  }

  // overflow.page: report the outermost element that pushes past the right edge.
  if (doc.scrollWidth > doc.clientWidth + 1 && ![doc, document.body].some((e) => /hidden|clip/.test(getComputedStyle(e).overflowX))) {
    const clippedX = (el) => { for (let e = el.parentElement; e && e !== document.body; e = e.parentElement) if (styleOf.get(e).overflowX !== 'visible') return true; return false; };
    const el = visible.find((e) => e.getBoundingClientRect().right > doc.clientWidth + 1 && !clippedX(e)) || document.body;
    add('overflow.page', el, `page ${doc.scrollWidth} px wide > viewport ${doc.clientWidth} px`);
  }

  // Tokens: custom properties on the body (inherited from :root) and in every readable stylesheet.
  const decls = [];
  const bodyStyle = getComputedStyle(document.body);
  for (const name of bodyStyle) if (name.startsWith('--')) decls.push([name, bodyStyle.getPropertyValue(name).trim()]);
  const scan = (rules) => {
    for (const r of rules) {
      if (r.style) for (const name of r.style) if (name.startsWith('--')) decls.push([name, r.style.getPropertyValue(name).trim()]);
      if (r.cssRules) scan(r.cssRules);
    }
  };
  for (const sheet of document.styleSheets) { try { scan(sheet.cssRules); } catch { /* ^ cross-origin sheet */ } }
  const ckey = (c) => rgba(c).join(',');
  const colorTokens = new Set(), spaceTokens = [];
  withProbe((probe) => {
    for (const [name, v] of decls) {
      probe.style.color = ''; probe.style.color = v;
      if (probe.style.color) colorTokens.add(ckey(getComputedStyle(probe).color));
      if (/space|spacing|gap|pad|inset|gutter|margin/i.test(name)) {
        probe.style.width = ''; probe.style.width = v;
        if (probe.style.width) spaceTokens.push(parseFloat(getComputedStyle(probe).width));
      }
    }
  });

  if (colorTokens.size) {
    for (const el of visible) {
      const s = styleOf.get(el);
      const ownText = [...el.childNodes].some((n) => n.nodeType === 3 && n.nodeValue.trim());
      const used = [['color', ownText && s.color], ['background-color', s.backgroundColor],
        ...SIDES.map((d) => [`border-${d.toLowerCase()}-color`, parseFloat(s[`border${d}Width`]) > 0 && s[`border${d}Color`]])];
      for (const [prop, c] of used) {
        if (c && rgba(c)[3] > 0 && !colorTokens.has(ckey(c))) add('color.raw', el, `${prop} ${c}`, { key: ckey(c) });
      }
    }
  }
  if (spaceTokens.length) {
    const SPACING = ['padding-top', 'padding-right', 'padding-bottom', 'padding-left', 'row-gap', 'column-gap'];
    const MARGINS = ['margin-top', 'margin-right', 'margin-bottom', 'margin-left'];
    // ^ Typed OM tells an auto margin from a length; without it margins are skipped.
    const typed = 'computedStyleMap' in Element.prototype, props = typed ? [...SPACING, ...MARGINS] : SPACING;
    for (const el of visible) {
      const s = styleOf.get(el);
      for (const prop of props) {
        const v = Math.abs(parseFloat(s.getPropertyValue(prop)));
        if (!(v > 2) || spaceTokens.some((t) => Math.abs(t - v) <= 0.5)) continue;
        if (MARGINS.includes(prop) && el.computedStyleMap().get(prop) instanceof CSSKeywordValue) continue;
        add('spacing.scale', el, `${prop} ${+v.toFixed(2)}px`, { key: v.toFixed(1) });
      }
    }
  }

  // Slop signals (reference/SLOP.md)
  const hues = {}; let paint = 0;
  for (const el of visible) {
    const s = styleOf.get(el), r = el.getBoundingClientRect(), area = Math.min(r.width * r.height, vw * vh);
    for (const [c, w] of [[s.backgroundColor, area], [s.color, el.childNodes.length ? 200 : 0], [parseFloat(s.borderTopWidth) ? s.borderTopColor : 'transparent', r.width * 2]]) {
      if (!w || rgba(c)[3] <= 0.05) continue;
      paint += w;
      const h = saturatedHue(c);
      if (h !== null) { const k = (Math.round(h / 30) % 12) * 30; hues[k] = (hues[k] || 0) + w; }
    }
  }
  const families = Object.entries(hues).filter(([, w]) => w / paint >= 0.002).sort((a, b) => b[1] - a[1]);
  if (families.length > 3) add('slop.palette', document.body, `${families.length} hue families: ${families.map(([h]) => h + 'deg').join(' ')}`);

  for (const el of visible) {
    const s = styleOf.get(el);
    if (s.animationName !== 'none' && /infinite/.test(s.animationIterationCount) && !el.closest('[aria-busy="true"], [role=progressbar]')) add('slop.motion', el, textOf(el));
    if (/blur\(/.test(s.backdropFilter || s.webkitBackdropFilter || '')) add('slop.glass', el, textOf(el));
  }
  const cards = visible.filter((el) => {
    const s = styleOf.get(el), r = el.getBoundingClientRect();
    return parseFloat(s.borderTopLeftRadius) >= 8 && (s.boxShadow !== 'none' || parseFloat(s.borderTopWidth) > 0) && r.width >= 120 && r.height >= 60 && el.children.length;
  });
  const cardSet = new Set(cards);
  for (const el of cards) {
    let outer = el.parentElement; while (outer && !cardSet.has(outer)) outer = outer.parentElement;
    if (outer) add('slop.nested-cards', el, `inside ${sel(outer)}`);
  }
  textHits('slop.emoji', /\p{Emoji_Presentation}|\p{Extended_Pictographic}️/u, false);
  textHits('slop.filler', /\b(built|made|crafted|designed|written) (with|in|from|using)\b|\bpowered by\b/i);
  textHits('slop.marketing', /\b(seamless(ly)?|elevate[sd]?|empower(s|ing)?|unlock|supercharge|effortless(ly)?|revolutioni[sz]e|next[- ]level|cutting[- ]edge|game[- ]chang(er|ing)|unleash|world[- ]class|all[- ]in[- ]one|welcome to)\b/i);

  const filled = controls.filter((el) => { const r = el.getBoundingClientRect(); return r.top < vh && r.bottom > 0 && saturatedHue(styleOf.get(el).backgroundColor) !== null; });
  if (filled.length > 1) add('slop.primary', document.body, `${filled.length} filled: ${filled.slice(0, 4).map(sel).join(', ')}`, { r: filled[1].getBoundingClientRect() });
  if (cfg.primary) {
    const el = document.querySelector(cfg.primary), r = el?.getBoundingClientRect();
    if (!r || r.top < 0 || r.bottom > vh || r.left < 0 || r.right > vw) add('primary.offscreen', el || document.body, el ? textOf(el) : `${cfg.primary} not found`);
  }

  // Layout (reference/LAYOUT.md): a short sibling beside a tall one leaves an empty block; short items far apart leave a gap.
  const beside = (a, b) => a.top < b.bottom && b.top < a.bottom;
  // ^ Height of what a child actually shows: flex and grid stretch an empty column to the row's height.
  const shownHeight = (k, r) => contentRect(k).height || r.height;
  for (const el of visible) {
    const s = styleOf.get(el);
    const row = (/flex/.test(s.display) && !s.flexDirection.startsWith('column')) || /grid/.test(s.display);
    const kids = row ? [...el.children].filter((k) => shown.has(k)) : [];
    if (kids.length < 2) continue;
    const box = el.getBoundingClientRect();
    const rects = kids.map((k) => { const r = k.getBoundingClientRect(); return [k, r, shownHeight(k, r)]; });
    const [tallEl, tall, tallH] = rects.reduce((a, b) => (b[2] > a[2] ? b : a));
    for (const [k, r, h] of rects) {
      if (k !== tallEl && tallH >= 48 && beside(r, tall) && h < tallH * 0.5 && r.width >= box.width * 0.15) add('layout.dead-space', k, `beside ${sel(tallEl)}, ${Math.round(r.width * (tallH - h))} px2 empty`, { r });
    }
    const line = rects.filter(([, r]) => r.height < 48).sort((a, b) => a[1].left - b[1].left);
    for (let i = 1; i < line.length; i++) {
      const [a, ra] = line[i - 1], [b, rb] = line[i], gap = rb.left - ra.right;
      if (beside(ra, rb) && gap > Math.max(120, box.width * 0.3)) add('layout.wide-gap', b, `${Math.round(gap)} px from ${sel(a)} in a ${Math.round(box.width)} px row`, { r: rb });
    }
  }

  // Phone rules (reference/MOBILE.md), touch only.
  if (touch) {
    // ^ Content share: the largest vertically scrolling region's visible height; menus over the content are not the content.
    const regions = visible.filter((el) => /(auto|scroll)/.test(styleOf.get(el).overflowY) && el.scrollHeight > el.clientHeight + 1 && !el.closest(LAYER))
      .map((el) => { const r = el.getBoundingClientRect(); return { el, h: Math.min(r.bottom, vh) - Math.max(r.top, 0), w: r.width }; })
      .filter((x) => x.w >= vw * 0.6 && x.h >= vh / 4).sort((a, b) => b.h - a.h);
    const main = regions[0], pageScrolls = document.scrollingElement.scrollHeight > vh + 1;
    // ^ A page that scrolls holds its own content; an inner region decides only in an app shell that does not scroll.
    const share = pageScrolls ? 1 : main ? main.h / vh : null;
    const need = document.activeElement?.matches?.('input, textarea, select, [contenteditable=""], [contenteditable=true]') ? 0.45 : 0.75;
    if (share !== null && share < need) add('mobile.content-share', main ? main.el : document.body, `content ${share.toFixed(2)} of the height < ${need}`);

    const scrollsX = (el) => /(auto|scroll)/.test(styleOf.get(el).overflowX) && el.scrollWidth > el.clientWidth + 1;
    for (const { p, t, rects } of texts) {
      const s = styleOf.get(p);
      if (parseFloat(s.fontSize) < 12) add('text.tiny', p, `${t} (${s.fontSize})`, { r: rects[0] });
      let e = p; while (e && e !== document.body && !scrollsX(e)) e = e.parentElement;
      if (e && e !== document.body) continue;
      // ^ rects are already cut to what the boxes show, so an ellipsized line is measured as far as it is drawn.
      const r = rects.find((r) => r.left < gutter - 1 || r.right > vw - gutter + 1);
      if (r) add('mobile.gutter', p, t, { r });
    }
    for (const el of visible) {
      if (el.matches('input:not([type=hidden]):not([type=checkbox]):not([type=radio]):not([type=file]), textarea, select, [contenteditable=""], [contenteditable=true]') && parseFloat(styleOf.get(el).fontSize) < 16) add('input.zoom', el, `${textOf(el)} (${styleOf.get(el).fontSize})`);
    }
  }

  findings.sort((a, b) => (a.level === b.level ? 0 : a.level === 'error' ? -1 : 1));
  const open = findings.filter((f) => !f.kept);
  const counts = { error: open.filter((f) => f.level === 'error').length, warn: open.filter((f) => f.level === 'warn').length, kept: findings.length - open.length };
  return { viewport: [vw, Math.round(vh)], touch, url: location.href, findings, counts, ok: counts.error === 0 };
})()
