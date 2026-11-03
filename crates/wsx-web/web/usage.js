// What the project's agents spent today and in 7 days, workers and manager apart, from the
// overview's `usage` (docs/web-client-tasks.md "Usage"); and one task's usage line.
import { html, useEffect, useState } from "./lib.js";
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

// The parts of a day's spend, in the order of their fixed colors (`usage.rs` PARTS).
const PARTS = [["build", "Build"], ["plan", "Plan"], ["context", "Context"], ["review", "Review"], ["memo", "Memo"], ["other", "Other work"], ["manager", "Manager"]];

// The kinds of token, in the order of their fixed colors (`app.css` .usage i[data-part]).
const KINDS = [["input", "Input"], ["cache_read", "Cache read"], ["cache_write", "Cache write"], ["output", "Output"]];

// A cost chart's and a token chart's own parts: what a day's bar splits into, its numbers and
// how they read. `extra` is a row's last cell: the cache hit rate, cache read / (input + cache read + cache write).
const COST = { name: "cost", title: "Cost", label: "Cost per day", legend: "Parts of the cost in 7 days", parts: PARTS, format: usd,
  of: (day) => day.parts };
const TOKENS = { name: "tokens", title: "Tokens", label: "Tokens per day", legend: "Kinds of token in 7 days", parts: KINDS, format: tokens,
  of: (day) => { const u = day.usage ?? {}; return { input: u.input_tokens, cache_read: u.cache_read_tokens, cache_write: u.cache_creation_tokens, output: u.output_tokens }; },
  extra: (amounts) => { const input = (amounts.input ?? 0) + (amounts.cache_read ?? 0) + (amounts.cache_write ?? 0); return input ? `${Math.round((100 * (amounts.cache_read ?? 0)) / input)}%` : "-"; } };

const sum = (amounts) => Object.values(amounts).reduce((total, amount) => total + (amount ?? 0), 0);

const partCost = (days, chart, part) => days.reduce((total, day) => total + (chart.of(day)[part] ?? 0), 0);

// A row's bar: the parts in their fixed order, as wide as `share` of the track.
const Bar = ({ chart, parts, share }) => html`
  <span class="usage-track"><span class="usage-bar" style=${{ width: `${share}%` }}>
    ${chart.parts.filter(([part]) => parts[part]).map(([part, label]) => html`<i key=${part} data-part=${part} title=${`${label} ${chart.format(parts[part])}`} style=${{ flexGrow: parts[part] }}></i>`)}
  </span></span>`;

// The 7 days at once, each part's amount and share in the legend, then one row per day, newest
// first, on one scale. A tap on a day shows its exact amounts (docs/web-client-tasks.md "Usage").
// The token chart's rows end with the cache hit rate: a small kind is never narrower than 4 px.
function DayRows({ days, chart }) {
  const [open, setOpen] = useState(null);
  const totals = days.map((day) => sum(chart.of(day)));
  const most = Math.max(1, ...totals);
  const parts = Object.fromEntries(chart.parts.map(([part]) => [part, partCost(days, chart, part)]));
  const total = sum(parts);
  const rate = (amounts) => chart.extra && html`<span class="usage-rate" title="Cache hit rate">${chart.extra(amounts)}</span>`;
  return html`
    <section class="usage-chart" data-chart=${chart.name} aria-label=${chart.title}>
      <div class="usage-total">
        <span class="usage-date">7 days</span>
        <${Bar} chart=${chart} parts=${parts} share=${total ? 100 : 0} />
        <span class="usage-cost">${chart.format(total)}</span>
        ${rate(parts)}
      </div>
      <ul class="usage-legend" aria-label=${chart.legend}>
        ${chart.parts.map(([part, label]) => html`<li key=${part}><i data-part=${part}></i><span class="usage-name">${label}</span><span class="usage-amount">${chart.format(parts[part])} ${total ? Math.round((100 * parts[part]) / total) : 0}%</span></li>`)}
      </ul>
      <ul class="usage-days" aria-label=${chart.label}>
        ${days.map((day, index) => {
          const amounts = chart.of(day);
          const cost = totals[index];
          const date = new Date(day.day_ms).toLocaleDateString(undefined, { weekday: "short", day: "numeric" });
          return html`
            <li key=${day.day_ms}>
              <button type="button" class="usage-day" aria-expanded=${open === day.day_ms} disabled=${!cost} onClick=${() => setOpen(open === day.day_ms ? null : day.day_ms)}>
                <span class="usage-date">${date}</span>
                <${Bar} chart=${chart} parts=${amounts} share=${(100 * cost) / most} />
                <span class="usage-cost">${chart.format(cost)}</span>
                ${rate(amounts)}
              </button>
              ${open === day.day_ms && html`
                <dl class="usage-parts">
                  ${chart.parts.filter(([part]) => amounts[part]).map(([part, label]) => html`<div key=${part}><dt><i data-part=${part}></i>${label}</dt><dd>${chart.format(amounts[part])}</dd></div>`)}
                </dl>`}
            </li>`;
        })}
      </ul>
    </section>`;
}

export function UsagePage({ project, onClose, grip }) {
  const usage = useStore().tasks[project]?.usage;
  useEffect(() => { grip.ready(); }, []);
  // ^ One chart at a time, Cost or Tokens, so each day shows once.
  const [chart, setChart] = useState(COST);
  const cells = usage && [usage.today.workers, usage.today.manager, usage.week.workers, usage.week.manager];
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose}><span class="bar-label">Usage</span></${SheetBar}>
      <div class="scroll usage">
        ${!cells ? html`<p class="empty">No usage yet.</p>` : html`
          <div class="usage-view" role="tablist" aria-label="Chart">
            ${[COST, TOKENS].map((each) => html`<button key=${each.name} type="button" role="tab" aria-selected=${chart === each} onClick=${() => setChart(each)}>${each.title}</button>`)}
          </div>
          <${DayRows} key=${chart.name} days=${usage.days ?? []} chart=${chart}/>
          <details class="usage-tokens"><summary>Tokens</summary><div class="table-wrap">
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
          </div></details>`}
      </div>
    </div>`;
}
