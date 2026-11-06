// A project's panels (docs/panels.md): a command wsxd runs, its output shown as written, or for a
// `format: json` panel with a good document, as cards with bars. The header
// icon exists only for a project that declares a panel and a device that may run commands; the page
// reads the last result at once, starts a run on open and on Refresh, and reads again when the run
// is done and when the panel's `refresh` is due.
import { api } from "./api.js";
import { html, Icon, Loading, useEffect, useNow, useState } from "./lib.js";
import { can, goBack, shows, state, update, useStore } from "./store.js";
import { SheetBar } from "./ui.js";

const url = (project, name = "", run = false) => `api/projects/${project}/panels${name ? `/${encodeURIComponent(name)}` : ""}${run ? "?run=1" : ""}`;

// "12 s ago", "3 min ago": the age of a result.
const age = (ms) => (ms < 60000 ? `${Math.floor(ms / 1000)} s ago` : `${Math.floor(ms / 60000)} min ago`);

export function PanelsButton({ project }) {
  const [names, setNames] = useState([]);
  useEffect(() => {
    if (!can("terminal_input")) return;
    api(url(project)).then((value) => setNames(value.panels ?? []), () => {});
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
      <div class="panel-row"><span class="panel-label">${item.label}</span><span class="panel-value">${value}</span><${Level} level=${item.level} /></div>
      <div class="panel-bar" role="meter" aria-label=${item.label} aria-valuemin="0" aria-valuemax=${item.total} aria-valuenow=${Math.min(item.used, item.total)} aria-valuetext=${value}>
        <span style=${{ width: `${Math.min(100, share * 100)}%` }}></span>
      </div>
    </div>`;
}

const Stat = ({ item }) => html`
  <div class="panel-row panel-stat" data-level=${item.level ?? null}>
    <span class="panel-label">${item.label}</span><strong class="panel-value">${item.value}</strong>
    ${item.note && html`<span class="panel-note">${item.note}</span>`}<${Level} level=${item.level} />
  </div>`;

const Table = ({ item }) => html`
  <div class="panel-table"><table>
    <thead><tr>${item.columns.map((column, at) => html`<th key=${at} scope="col">${column}</th>`)}</tr></thead>
    <tbody>${item.rows.map((row, at) => html`<tr key=${at}>${row.map((cell, index) => html`<td key=${index}>${cell}</td>`)}</tr>`)}</tbody>
  </table></div>`;

const List = ({ item }) => html`
  <div class="panel-list" data-level=${item.level ?? null}>
    <ul>${item.items.map((entry, at) => html`<li key=${at}>${entry}</li>`)}</ul><${Level} level=${item.level} />
  </div>`;

const ITEMS = { gauge: Gauge, stat: Stat, table: Table, list: List };

// A good document: its title, then a card per section.
const Doc = ({ doc }) => html`
  <div class="panel-doc">
    ${doc.title && html`<h2 class="panel-title">${doc.title}</h2>`}
    ${doc.sections.map((section, at) => html`
      <section class="panel-card" key=${at}>
        ${section.title && html`<h3>${section.title}</h3>`}
        ${section.items.map((item, index) => { const Item = ITEMS[item.kind]; return html`<${Item} key=${index} item=${item} />`; })}
      </section>`)}
  </div>`;

// One panel: its last result, kept fresh while it is shown.
function Panel({ project, name }) {
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
        const value = await api(url(project, name, run));
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
  }, [project, name, again]);
  if (failure && !view) return html`<p class="error empty" role="alert">${failure}</p>`;
  if (!view) return html`<${Loading} />`;
  const { run } = view;
  const failed = run.status !== "ok" && run.status !== "pending";
  const how = { failed: `failed (exit ${run.exit ?? "signal"})`, timed_out: "timed out", not_started: "did not start" }[run.status];
  return html`
    <div class="panel" data-status=${run.status}>
      <p class="panel-state" data-failed=${failed ? "" : null}>
        ${run.status === "pending" ? "Running…" : html`${how ?? "ok"}, updated ${age(view.age_ms + (now - read_at))}`}
        ${view.refresh ? ` · every ${view.refresh} s` : " · on open"}
        <button type="button" class="link-action" disabled=${view.running} onClick=${() => setAgain(again + 1)}>${view.running ? "Running…" : "Refresh"}</button>
      </p>
      ${failure && html`<p class="error" role="alert">${failure}</p>`}
      ${run.truncated && html`<p class="note">The start of the output is cut.</p>`}
      ${run.notice && html`<p class="note panel-notice">${run.notice}</p>`}
      ${run.doc ? html`<${Doc} doc=${run.doc} />`
        : html`<pre class="panel-output" aria-label=${`Output of ${name}`}>${run.output || (run.status === "pending" ? "" : "(no output)")}</pre>`}
    </div>`;
}

// The page: the project's panels, one at a time.
export function PanelsPage({ project, onClose, grip }) {
  const [list, setList] = useState(null);
  const [failure, setFailure] = useState("");
  const [open, setOpen] = useState(null);
  useEffect(() => {
    api(url(project)).then((value) => { setList(value); setOpen((was) => was ?? value.panels?.[0]?.name ?? null); }, (error) => setFailure(error.message));
  }, [project]);
  useEffect(() => { if (list || failure) grip?.ready?.(); }, [Boolean(list), failure]);
  const panels = list?.panels ?? [];
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose}><span class="bar-label">Panels</span></${SheetBar}>
      <div class="scroll panels">
        ${failure ? html`<p class="error empty" role="alert">${failure}</p>`
          : !list ? html`<${Loading} />`
          : html`
            ${list.notice && html`<p class="note">${list.notice}</p>`}
            ${!panels.length ? html`<p class="empty">No panels</p>`
              : html`
                ${panels.length > 1 && html`<div class="panel-tabs" role="tablist" aria-label="Panels">
                  ${panels.map((panel) => html`<button type="button" role="tab" key=${panel.name} aria-selected=${open === panel.name} onClick=${() => setOpen(panel.name)}>${panel.name}</button>`)}
                </div>`}
                ${open && html`<${Panel} key=${open} project=${project} name=${open} />`}`}`}
      </div>
    </div>`;
}

// The machine panels of the granted projects (docs/panels.md "Machine panels"): read at the start,
// after a missed event and when the project list changes. A failed read keeps the last list.
export async function loadMachine() {
  if (!can("terminal_input") || !shows("terminal")) return;
  try {
    const { projects } = await api("api/machine/panels");
    if (JSON.stringify(projects) !== JSON.stringify(state.machine)) update({ machine: projects });
  } catch { /* the tab keeps what it had */ }
}

// The Machine tab: every machine panel, its name over the project's, each with its own run and refresh.
export function MachineScreen() {
  const { machine, loaded } = useStore();
  useEffect(() => { loadMachine(); }, []);
  const panels = machine.flatMap((project) => project.panels.map((panel) => ({ project, panel })));
  return html`
    <div class="page">
      <h1 class="title">Machine</h1>
      <div class="scroll panels">
        ${!panels.length ? html`<p class="empty">${loaded ? "No machine panels" : ""}</p>`
          : panels.map(({ project, panel }) => html`
            <section class="machine-panel" key=${`${project.id}/${panel.name}`} aria-label=${panel.name}>
              <h2 class="machine-name">${panel.name}<small>${project.name}</small></h2>
              <${Panel} project=${project.id} name=${panel.name} />
            </section>`)}
      </div>
    </div>`;
}

// The same page as a screen, for a site without the manager (no task_view).
export const PanelsScreen = ({ project }) => html`<${PanelsPage} project=${project} onClose=${() => goBack(`#/p/${project}`)} />`;
