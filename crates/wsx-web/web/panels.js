// A project's panels (docs/panels.md): a command wsxd runs, its output shown as written, or for a
// `format: json` panel with a good document, as cards with bars. The header
// icon exists only for a project that declares a panel and a device that may run commands; the page
// reads the last result at once, starts a run on open and on Refresh, and reads again when the run
// is done and when the panel's `refresh` is due.
import { api } from "./api.js";
import { html, Icon, Loading, useEffect, useNow, useState } from "./lib.js";
import { can, goBack, shows, state, useStore } from "./store.js";
import { SheetPage, useSheetRead } from "./ui.js";

// `base`: a project's panels, or the machine card (api/machine).
const projectPanels = (project) => `api/projects/${project}/panels`;
const url = (base, name = "", run = false) => `${base}${name ? `/${encodeURIComponent(name)}` : ""}${run ? "?run=1" : ""}`;

// "12 s ago", "3 min ago": the age of a result.
// ^ The clock tick can be older than the read that was just made: never below 0 s.
const age = (ms) => (ms < 60000 ? `${Math.max(0, Math.floor(ms / 1000))} s ago` : `${Math.floor(ms / 60000)} min ago`);

export function PanelsButton({ project }) {
  const [names, setNames] = useState([]);
  useEffect(() => {
    if (!can("terminal_input")) return;
    api(url(projectPanels(project))).then((value) => setNames(value.panels ?? []), () => {});
  }, [project]);
  if (!names.length) return null;
  return html`<a class="icon-button" href=${`#/p/${project}/panels`} aria-label="Panels" title="Panels"><${Icon} name="panel" /></a>`;
}

// 8.5, 16: one decimal, none when whole.
const num = (value) => String(Math.round(value * 10) / 10);

// A level is the theme's state colour and its word, never the colour alone.
const Level = ({ level }) => (level ? html`<span class="panel-level" data-level=${level}>${level}</span>` : null);

function Gauge({ item }) {
  const share = item.used / item.total;
  const unit = item.unit ? ` ${item.unit}` : "";
  const value = `${num(item.used)} / ${num(item.total)}${unit} · ${Math.round(share * 100)}%`;
  return html`
    <div class="panel-gauge" data-level=${item.level ?? null}>
      <div class="panel-row"><span class="panel-label">${item.label}</span><span class="panel-end"><span class="panel-value">${value}</span><${Level} level=${item.level} /></span></div>
      <div class="panel-bar" role="meter" aria-label=${item.label} aria-valuemin="0" aria-valuemax=${item.total} aria-valuenow=${Math.min(item.used, item.total)} aria-valuetext=${value}>
        <span style=${{ width: `${Math.min(100, share * 100)}%` }}></span>
      </div>
    </div>`;
}

const Stat = ({ item }) => html`
  <div class="panel-row panel-stat" data-level=${item.level ?? null}>
    <span class="panel-label">${item.label}</span>
    <span class="panel-end"><strong class="panel-value">${item.value}</strong>${item.note && html`<span class="panel-note">${item.note}</span>`}<${Level} level=${item.level} /></span>
  </div>`;

// Where a link goes (docs/panels.md "Links"): an https address in a new tab; a project page in the
// files view of that project, which shows it in the sandboxed frame (docs/web-client.md "Files").
// A site that may not read files cannot follow a page, so it gets null and shows the text alone.
// The machine card names the project of a path (`owner`, a project's name); a project's page does not.
function destination(url, owner, project) {
  if (/^https?:\/\//.test(url)) return { href: url, web: true };
  const id = owner ? state.projects.find((candidate) => candidate.name === owner)?.id : project;
  if (id === undefined || !shows("files") || !can("files_read")) return null;
  return { href: `#/p/${id}/files/${url.split("/").map(encodeURIComponent).join("/")}`, web: false };
}

// A table cell or list entry: its text, and a link when the document gave one and the site can follow it.
function Linked({ text, link, project }) {
  const to = link && destination(link.url, link.project, project);
  if (!to) return text;
  return html`<a class="panel-link" href=${to.href} target=${to.web ? "_blank" : null} rel=${to.web ? "noopener noreferrer" : null} title=${link.url}>${text}</a>`;
}

const linkAt = (links, row, col) => links?.find((link) => link.row === row && link.col === col);

const Table = ({ item, project }) => html`
  <div class="panel-table"><table>
    <thead><tr>${item.columns.map((column, at) => html`<th key=${at} scope="col">${column}</th>`)}</tr></thead>
    <tbody>${item.rows.map((row, at) => html`<tr key=${at}>${row.map((cell, index) => html`<td key=${index}><${Linked} text=${cell} link=${linkAt(item.links, at, index)} project=${project} /></td>`)}</tr>`)}</tbody>
  </table></div>`;

const List = ({ item, project }) => html`
  <div class="panel-list" data-level=${item.level ?? null}>
    <ul>${item.items.map((entry, at) => html`<li key=${at}><${Linked} text=${entry} link=${linkAt(item.links, at, 0)} project=${project} /></li>`)}</ul><${Level} level=${item.level} />
  </div>`;

// A card's picture comes from the files path (`api/file/image`, the right to read files); a site without it shows the card
// without the picture. It loads when it scrolls near and sits in a box of fixed shape, so the grid does not jump.
const imageOf = (card, project) => {
  if (!card.image || !shows("files") || !can("files_read")) return null;
  const id = card.project ? state.projects.find((candidate) => candidate.name === card.project)?.id : project;
  return id === undefined ? null : `api/file/image?project=${id}&path=${encodeURIComponent(card.image)}`;
};

function Tile({ card, project }) {
  const to = card.url ? destination(card.url, card.project, project) : null;
  const src = imageOf(card, project);
  const body = html`
    ${src && html`<div class="panel-tile-image"><img src=${src} alt="" loading="lazy" decoding="async" onError=${(event) => { event.target.parentElement.hidden = true; }} /></div>`}
    <div class="panel-tile-text">
      <strong class="panel-tile-title">${card.title}</strong>
      ${card.subtitle && html`<span class="panel-tile-subtitle">${card.subtitle}</span>`}
      ${(card.value || card.level) && html`<span class="panel-tile-value">${card.value && html`<strong>${card.value}</strong>`}<${Level} level=${card.level} /></span>`}
      ${card.note && html`<span class="panel-tile-note">${card.note}</span>`}
    </div>`;
  return to
    ? html`<a class="panel-tile" data-level=${card.level ?? null} href=${to.href} target=${to.web ? "_blank" : null} rel=${to.web ? "noopener noreferrer" : null} title=${card.url}>${body}</a>`
    : html`<div class="panel-tile" data-level=${card.level ?? null}>${body}</div>`;
}

const Cards = ({ item, project }) => html`
  <div>
    ${item.title && html`<h4 class="panel-cards-title">${item.title}</h4>`}
    <div class="panel-cards">${item.items.map((card, at) => html`<${Tile} key=${at} card=${card} project=${project} />`)}</div>
  </div>`;

const ITEMS = { gauge: Gauge, stat: Stat, table: Table, list: List, cards: Cards };

// A good document: its title, then a card per section.
// ^ `doc.level` is set only on the machine card: the worst warn or bad of its items.
const Doc = ({ doc, project }) => html`
  <div class="panel-doc">
    ${doc.title && html`<h2 class="panel-title" data-level=${doc.level ?? null}>${doc.title}<${Level} level=${doc.level} /></h2>`}
    ${doc.sections.map((section, at) => html`
      <section class="panel-card" key=${at}>
        ${section.title && html`<h3>${section.title}</h3>`}
        ${section.items.map((item, index) => { const Item = ITEMS[item.kind]; return html`<${Item} key=${index} item=${item} project=${project} />`; })}
      </section>`)}
  </div>`;

// One panel: its last result, kept fresh while it is shown.
function Panel({ base, name, project }) {
  const [view, setView] = useState(null);
  const [failure, setFailure] = useState("");
  const [again, setAgain] = useState(0);
  const [read_at, setReadAt] = useState(0);
  const now = useNow(true);
  useEffect(() => {
    let live = true;
    let timer = null;
    const read = async (run) => {
      // ^ A hidden page shows nothing: no read, so no run.
      if (document.hidden) { timer = setTimeout(() => read(false), 5000); return; }
      try {
        const value = await api(url(base, name, run));
        if (!live) return;
        setView(value);
        setReadAt(Date.now());
        setFailure("");
        const wait = value.running || value.run.status === "pending" ? 1000
          : value.refresh ? Math.max(1000, value.refresh * 1000 - value.age_ms) : 0;
        if (wait) timer = setTimeout(() => read(false), wait);
      } catch (error) {
        if (!live) return;
        setFailure(error.message);
        timer = setTimeout(() => read(false), 10000);
      }
    };
    read(true);
    return () => { live = false; clearTimeout(timer); };
  }, [base, name, again]);
  if (failure && !view) return html`<p class="error empty" role="alert">${failure}</p>`;
  if (!view) return html`<${Loading} />`;
  const { run } = view;
  const failed = run.status !== "ok" && run.status !== "pending";
  const how = { failed: `Failed (exit ${run.exit ?? "signal"})`, timed_out: "Timed out", not_started: "Did not start" }[run.status];
  const when = run.status === "pending" ? "No result yet" : `${how ? `${how}, updated` : "Updated"} ${age(view.age_ms + (now - read_at))}`;
  const running = view.running || run.status === "pending";
  // ^ The status words, then Refresh at the row's end: it says Running… from the tap until the run ends.
  return html`
    <div class="panel" data-status=${run.status}>
      <div class="panel-state" data-failed=${failed ? "" : null}>
        <p class="panel-state-text">${when}, ${view.refresh ? `every ${view.refresh} s` : "on open"}</p>
        <button type="button" class="button panel-refresh" disabled=${running} onClick=${() => { setView({ ...view, running: true }); setAgain(again + 1); }}>${running ? "Running…" : "Refresh"}</button>
      </div>
      ${failure && html`<p class="error" role="alert">${failure}</p>`}
      ${run.truncated && html`<p class="note">The start of the output is cut.</p>`}
      ${run.notice && html`<p class="note panel-notice">${run.notice}</p>`}
      ${run.doc ? html`<${Doc} doc=${run.doc} project=${project} />`
        : html`<pre class="panel-output" aria-label=${`Output of ${name || "the machine"}`}>${run.output || (run.status === "pending" ? "" : "(no output)")}</pre>`}
    </div>`;
}

// The page: the project's panels, one at a time.
export function PanelsPage({ project, onClose, grip }) {
  const [list, failure] = useSheetRead(projectPanels(project));
  const [open, setOpen] = useState(null);
  const panels = list?.panels ?? [];
  // ^ The first panel until another is chosen.
  const shown = open ?? panels[0]?.name ?? null;
  return html`
    <${SheetPage} grip=${grip} onClose=${onClose} bar=${html`<span class="bar-label">Panels</span>`} className="panels" loaded=${Boolean(list)} failure=${failure}>
      ${list?.notice && html`<p class="note">${list.notice}</p>`}
      ${!panels.length ? html`<p class="empty">No panels</p>`
        : html`
          ${panels.length > 1 && html`<div class="panel-tabs" role="tablist" aria-label="Panels">
            ${panels.map((panel) => html`<button type="button" role="tab" key=${panel.name} aria-selected=${shown === panel.name} onClick=${() => setOpen(panel.name)}>${panel.name}</button>`)}
          </div>`}
          ${shown && html`<${Panel} key=${shown} base=${projectPanels(project)} name=${shown} project=${project} />`}`}
    </${SheetPage}>`;
}

// The Machine tab: one card, This Mac, with the sections of the granted projects' machine panels after
// its own (docs/panels.md "Machine panels"). The card is read again when the project list changes.
export function MachineScreen() {
  const { projects } = useStore();
  return html`
    <div class="page">
      <h1 class="title">Machine</h1>
      <div class="scroll panels">
        <section class="machine-panel" aria-label="Machine">
          <${Panel} key=${projects.map((project) => project.id).join()} base="api/machine" name="" />
        </section>
      </div>
    </div>`;
}

// The same page as a screen, for a site without the manager (no task_view).
export const PanelsScreen = ({ project }) => html`<${PanelsPage} project=${project} onClose=${() => goBack(`#/p/${project}`)} />`;
