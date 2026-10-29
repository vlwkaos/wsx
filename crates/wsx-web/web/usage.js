// What the project's agents spent today and in 7 days, workers and manager apart, from the
// overview's `usage` (docs/web-client-tasks.md "Usage"); and one task's usage line.
import { html, useEffect } from "./lib.js";
import { useStore } from "./store.js";
import { SheetBar } from "./ui.js";

export const usd = (micro) => `$${(micro / 1e6).toFixed(2)}`;

// "950", "12k", "1.2M", as `wsx_core::task::usage::tokens`.
export const tokens = (count) => count < 1e3 ? String(count) : count < 1e6 ? `${Math.floor(count / 1e3)}k` : `${(count / 1e6).toFixed(1)}M`;

// "$3.42 · in 12k · cache 1.2M read / 80k write · out 9k · 14 turns", as `Usage::line`.
export const usageLine = (usage) => `${usd(usage.cost_micro_usd)} · in ${tokens(usage.input_tokens)} · cache ${tokens(usage.cache_read_tokens)} read / ${tokens(usage.cache_creation_tokens)} write · out ${tokens(usage.output_tokens)} · ${usage.turns} turn${usage.turns === 1 ? "" : "s"}`;

const ROWS = [
  ["Cost", (usage) => usd(usage.cost_micro_usd)],
  ["Input", (usage) => tokens(usage.input_tokens)],
  ["Cache read", (usage) => tokens(usage.cache_read_tokens)],
  ["Cache write", (usage) => tokens(usage.cache_creation_tokens)],
  ["Output", (usage) => tokens(usage.output_tokens)],
  ["Turns", (usage) => String(usage.turns)],
];

export function UsagePage({ project, onClose, grip }) {
  const usage = useStore().tasks[project]?.usage;
  useEffect(() => { grip.ready(); }, []);
  const cells = usage && [usage.today.workers, usage.today.manager, usage.week.workers, usage.week.manager];
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose}><span class="bar-label">Usage</span></${SheetBar}>
      <div class="scroll usage">
        ${!cells ? html`<p class="empty">No usage yet.</p>` : html`
          <div class="table-wrap">
            <table aria-label="Token use and cost">
              <thead>
                <tr><td></td><th colspan="2" scope="colgroup">Today</th><th colspan="2" scope="colgroup">7 days</th></tr>
                <tr><td></td>${["Workers", "Manager", "Workers", "Manager"].map((who, index) => html`<th key=${index} scope="col" data-align="right">${who}</th>`)}</tr>
              </thead>
              <tbody>
                ${ROWS.map(([label, value]) => html`
                  <tr key=${label}><th scope="row">${label}</th>${cells.map((cell, index) => html`<td key=${index} data-align="right">${value(cell)}</td>`)}</tr>`)}
              </tbody>
            </table>
          </div>`}
      </div>
    </div>`;
}
