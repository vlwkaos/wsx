// The four trays of a project (Running, Needs you, To check, Routines): their icon, count and
// tone. The composer shows them as pills that open lists (tasks.js `Pills`), Home as icons beside
// a row's name (`TrayIcons`): one definition, so the two agree (docs/web-client-tasks.md
// "Project screen", "Home").
import { html, Icon } from "./lib.js";
import { routineCounts, routineWords } from "./routines.js";

// A count of 0 has no pill, except Routines, which shows while there is any routine (its count is
// the enabled ones; its tone is the worst last run).
export const TRAYS = [
  { key: "running", icon: "play", label: "Running", count: (c) => c.running, words: () => "running" },
  { key: "needs", icon: "unchecked", label: "Needs you", count: (c) => c.needs, words: (count) => count === 1 ? "needs you" : "need you" },
  { key: "check", icon: "pass", label: "To check", count: (c) => c.check, words: () => "to check" },
  { key: "routines", icon: "routines", label: "Routines", count: (c) => c.routines.on, shown: (c) => c.routines.total > 0, words: (count, c) => routineWords(c.routines), tone: (c) => c.routines.tone },
];
export const present = (counts, tray) => tray.shown ? tray.shown(counts) : tray.count(counts) > 0;
export const shownTrays = (counts) => TRAYS.filter((tray) => present(counts, tray));
export const trayWords = (tray, counts) => tray.key === "routines" ? tray.words(0, counts) : `${tray.count(counts)} ${tray.words(tray.count(counts), counts)}`;

// The counts of a project from its overview (the server's `working`, `waiting`, `needs`, `asks`,
// `to_check`); `routines` are the server's `routines` counts, newer ones when the caller has
// them. A chat's row has its own `running`, `needs` and `check` and no routines.
export function trayCounts(overview, routines = overview?.routines) {
  return {
    running: (overview?.working ?? 0) + (overview?.waiting?.length ?? 0),
    needs: (overview?.needs?.length ?? 0) + (overview?.asks?.length ?? 0),
    check: overview?.to_check ?? 0,
    routines: routineCounts(routines),
  };
}
export const chatCounts = (chat) => ({ running: chat.running, needs: chat.needs, check: chat.check, routines: routineCounts(null) });

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
