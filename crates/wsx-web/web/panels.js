// A project's panels (docs/panels.md): a command wsxd runs, its output shown as written. The header
// icon exists only for a project that declares a panel and a device that may run commands; the page
// reads the last result at once, starts a run on open and on Refresh, and reads again when the run
// is done and when the panel's `refresh` is due.
import { api } from "./api.js";
import { html, Icon, Loading, useEffect, useNow, useState } from "./lib.js";
import { can, leaveFor } from "./store.js";
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
      <pre class="panel-output" aria-label=${`Output of ${name}`}>${run.output || (run.status === "pending" ? "" : "(no output)")}</pre>
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

// The same page as a screen, for a site without the manager (no task_view).
export const PanelsScreen = ({ project }) => html`<${PanelsPage} project=${project} onClose=${() => leaveFor(`#/p/${project}`)} />`;
