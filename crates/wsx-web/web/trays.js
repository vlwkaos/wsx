// The three trays of a project (For you, Tasks, Routines): their icon, count and
// tone. The composer shows them as pills that open lists (tasks.js `Pills`), Home as icons beside
// a row's name (`TrayIcons`): one definition, so the two agree (docs/web-client-tasks.md
// "Project screen", "Home").
import { html, Icon } from "./lib.js";
import { routineCounts, routineWords } from "./routines.js";

// A count of 0 has no pill, except Routines, which shows while there is any routine (its count is
// the enabled ones; its tone is the worst last run).
export const TRAYS = [
  // ^ For you: what the owner answers, approves or checks (the server's `needs` and `check`); Tasks: every open task.
  { key: "you", icon: "unchecked", label: "For you", count: (c) => c.needs + c.check, words: (c) => `${c.needs + c.check} for you` },
  { key: "tasks", icon: "play", label: "Tasks", count: (c) => c.open, words: (c) => `${c.open} ${c.open === 1 ? "task" : "tasks"}` },
  { key: "routines", icon: "routines", label: "Routines", count: (c) => c.routines.on, shown: (c) => c.routines.total > 0, words: (c) => routineWords(c.routines), tone: (c) => c.routines.tone },
];
export const present = (counts, tray) => tray.shown ? tray.shown(counts) : tray.count(counts) > 0;
export const shownTrays = (counts) => TRAYS.filter((tray) => present(counts, tray));
export const trayWords = (tray, counts) => tray.words(counts);

// The counts of the trays: the server's `counts` of a project's overview or of a chat's row
// (wsx-core `ChatCounts`), and the project's `routines` counts, newer ones when the caller has
// them. A chat's row has no routines.
const NONE = { running: 0, open: 0, needs: 0, check: 0 };
export const trayCounts = (source, routines = source?.routines) => ({ ...(source?.counts ?? NONE), routines: routineCounts(routines) });

// The icons beside a Home row's name: no button, the row is the link.
export function TrayIcons({ counts }) {
  const shown = shownTrays(counts);
  if (!shown.length) return null;
  return html`
    <span class="trays">
      ${shown.map((tray) => html`
        <span class="tray" key=${tray.key} data-kind=${tray.key} data-tone=${tray.tone?.(counts) ?? null} role="img" aria-label=${trayWords(tray, counts)}>
          <${Icon} name=${tray.icon} /><span class="tray-count" key=${tray.count(counts)}>${tray.count(counts)}</span>
        </span>`)}
    </span>`;
}
