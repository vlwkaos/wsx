// The project's routines, read only, in the sheet the ⋯ menu opens: one row each, a tap shows
// the command (docs/web-client-tasks.md "Routines").
import { api } from "./api.js";
import { ago, clock, html, Loading, useCoalescedLoad, useEffect, useNow, useState } from "./lib.js";
import { SheetBar } from "./ui.js";

const DAYS = ["Sundays", "Mondays", "Tuesdays", "Wednesdays", "Thursdays", "Fridays", "Saturdays"];

// "0 6 * * *" is "daily 06:00", "0 6 * * 1" "Mondays 06:00"; any other cron stays as written.
function schedule(trigger) {
  const match = /^(\d{1,2}) (\d{1,2}) \* \* (\*|[0-7])$/.exec(trigger);
  if (!match || Number(match[1]) > 59 || Number(match[2]) > 23) return trigger;
  const time = `${match[2].padStart(2, "0")}:${match[1].padStart(2, "0")}`;
  return match[3] === "*" ? `daily ${time}` : `${DAYS[Number(match[3]) % 7]} ${time}`;
}

const STATUS = { succeeded: "succeeded", failed: "failed", spawn_failed: "did not start", cancelled: "cancelled", interrupted: "interrupted", running: "running" };

// "succeeded 2h ago", "failed (exit 1) 5h ago", "never run".
function lastRun(last, now) {
  if (!last) return "never run";
  const what = STATUS[last.status] ?? last.status;
  const exit = last.status === "failed" && last.exit != null ? ` (exit ${last.exit})` : "";
  const end = last.finished_unix_ms;
  return `${what}${exit}${end ? ` ${ago(now - end)}` : ""}`;
}

export function RoutinesPage({ project, onClose, grip }) {
  const [list, setList] = useState(null);
  const [failure, setFailure] = useState("");
  const [open, setOpen] = useState(null);
  const now = useNow(true);
  const read = useCoalescedLoad(async (current) => {
    const value = await api(`api/tasks/${project}/routines`);
    if (!current()) return;
    setList(value);
    setFailure("");
  }, (error) => setFailure(error.message), [project]);
  useEffect(() => { read(); }, [project]);
  useEffect(() => { if (list || failure) grip.ready(); }, [Boolean(list), failure]);
  const rows = list?.routines ?? [];
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose}><span class="bar-label">Routines</span></${SheetBar}>
      <div class="scroll routines">
        ${failure ? html`<p class="error empty" role="alert">${failure}</p>`
          : !list ? html`<${Loading} />`
          : list.error ? html`<p class="error empty" role="alert">${list.error}</p>`
          : !rows.length ? html`<p class="empty">No routines</p>`
          : html`<ul class="routine-list" aria-label="Routines">
            ${rows.map((row) => html`
              <li key=${row.name}>
                <button type="button" class="routine" aria-expanded=${open === row.name} onClick=${() => setOpen(open === row.name ? null : row.name)}>
                  <span class="routine-name">${row.name}</span>
                  <span class="routine-state" data-on=${row.enabled ? "" : null}>${row.enabled ? "On" : "Off"}</span>
                  <span class="routine-when">${schedule(row.trigger)}${row.post === false ? ", no chat post" : ""}</span>
                  <span class="routine-next">${row.next_run_unix_ms ? `next ${clock(row.next_run_unix_ms)}` : "no next run"}</span>
                  <span class="routine-last" data-failed=${row.last && row.last.status !== "succeeded" && row.last.status !== "running" ? "" : null}>${lastRun(row.last, now)}</span>
                </button>
                ${open === row.name && html`<code class="routine-argv">${row.command.join(" ")}</code>`}
              </li>`)}
          </ul>`}
      </div>
    </div>`;
}
