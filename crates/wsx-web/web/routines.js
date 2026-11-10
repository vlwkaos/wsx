// The project's routines: the data the Routines pill and its list read, and the list's rows
// (docs/web-client-tasks.md "Routines"). The pill and the list are in tasks.js.
import { api } from "./api.js";
import { ago, html, Icon, useCoalescedLoad, useEffect, useState } from "./lib.js";

const DAYS = ["Sundays", "Mondays", "Tuesdays", "Wednesdays", "Thursdays", "Fridays", "Saturdays"];

// "0 6 * * *" is "daily 06:00", "0 6 * * 1" "Mondays 06:00"; any other cron stays as written.
function schedule(trigger) {
  const match = /^(\d{1,2}) (\d{1,2}) \* \* (\*|[0-7])$/.exec(trigger);
  if (!match || Number(match[1]) > 59 || Number(match[2]) > 23) return trigger;
  const time = `${match[2].padStart(2, "0")}:${match[1].padStart(2, "0")}`;
  return match[3] === "*" ? `daily ${time}` : `${DAYS[Number(match[3]) % 7]} ${time}`;
}

const STATUS = { succeeded: "succeeded", findings: "found something", failed: "failed", spawn_failed: "did not start", cancelled: "cancelled", interrupted: "interrupted", running: "running" };

// "succeeded 2h ago", "failed (exit 1) 5h ago", "found something (exit 1) 5h ago", "never run".
function lastRun(last, now) {
  if (!last) return "never run";
  const what = STATUS[last.status] ?? last.status;
  const exit = (last.status === "failed" || last.status === "findings") && last.exit != null ? ` (exit ${last.exit})` : "";
  const end = last.finished_unix_ms;
  return `${what}${exit}${end ? ` ${ago(now - end)}` : ""}`;
}

const foundRun = (last) => last?.status === "findings";
const failedRun = (last) => Boolean(last) && last.status !== "succeeded" && last.status !== "running" && !foundRun(last);

// The routines of a project, read now, every minute, and again when `key` changes (the newest
// chat row: a run's end comes as a message, and it carries the run's message id), and every
// 2 s while a routine runs. `setList` takes the answer of an action.
export function useRoutines(project, active, key) {
  const [list, setList] = useState(null);
  const read = useCoalescedLoad(async (current) => {
    const next = await api(`api/tasks/${project}/routines`);
    // ^ An unchanged answer keeps the list as it was: the whole manager screen would draw again for nothing.
    if (current()) setList((old) => JSON.stringify(old) === JSON.stringify(next) ? old : next);
  }, () => {}, [project]);
  // ^ A routine that runs is read again every 2 s, so its end shows without a tap.
  const running = Boolean(list?.routines?.some((row) => row.running || row.last?.status === "running"));
  useEffect(() => {
    if (!active) return undefined;
    read();
    const timer = setInterval(read, running ? 2000 : 60000);
    return () => clearInterval(timer);
  }, [project, active, key, running]);
  return [list, setList];
}

// What the pill says: the enabled routines, how many failed or found something on their last
// run, and the tone that shows (the worst state wins).
export function routineCounts(list) {
  const rows = list?.routines ?? [];
  const on = rows.filter((row) => row.enabled);
  const failed = on.filter((row) => failedRun(row.last)).length;
  const found = on.filter((row) => foundRun(row.last)).length;
  return { total: rows.length, on: on.length, failed, found, tone: failed ? "danger" : found ? "attention" : null };
}

export function routineWords({ on, failed, found }) {
  const state = failed ? `, ${failed} failed` : found ? `, ${found} found something` : "";
  return `${on} ${on === 1 ? "routine" : "routines"}${state}`;
}

// The row's parts for ListRow: a tone dot for the last run, the label, two context lines (the
// schedule and whether it is on; the last run), the message to
// open, and the actions.
export function routineRow(row, now) {
  const last = row.last;
  const tone = !row.enabled ? null : failedRun(last) ? "danger" : foundRun(last) ? "attention" : last?.status === "running" ? "working" : "accent";
  const message = last?.message ?? null;
  const where = `${schedule(row.trigger)}${row.post === false ? ", no chat post" : ""} · ${row.enabled ? "On" : "Off"}`;
  return { tone, message, context: [where, lastRun(last, now)] };
}

// A routine's actions: turn it on or off, and run it now.
export function RoutineActions({ row, busy, onAct }) {
  const button = (label, icon, verb) => html`
    <button class="button icon-button need-action" type="button" title=${label} aria-label=${`${label}: ${row.name}`} disabled=${busy} onClick=${() => onAct(verb)}><${Icon} name=${icon} /></button>`;
  return html`${button(row.enabled ? "Turn off" : "Turn on", row.enabled ? "pause" : "play", row.enabled ? "disable" : "enable")}${button("Run now", "bolt", "run")}`;
}

export const routineAct = (project, name, verb) => api(`api/tasks/${project}/routines/${encodeURIComponent(name)}/${verb}`, { method: "POST", json: {} });
