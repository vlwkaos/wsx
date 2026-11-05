// What the project's agents spent, filtered by period, hands and agent, from
// `api/tasks/<project>/usage` (docs/web-client-tasks.md "Usage"); and one task's usage line.
import { api } from "./api.js";
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
const COST = { name: "cost", title: "Cost", label: "Cost per day", legend: "Parts of the cost", parts: PARTS, format: usd,
  of: (day) => day.parts };
const TOKENS = { name: "tokens", title: "Tokens", label: "Tokens per day", legend: "Kinds of token", parts: KINDS, format: tokens,
  of: (day) => { const u = day.usage ?? {}; return { input: u.input_tokens, cache_read: u.cache_read_tokens, cache_write: u.cache_creation_tokens, output: u.output_tokens }; },
  extra: (amounts) => { const input = (amounts.input ?? 0) + (amounts.cache_read ?? 0) + (amounts.cache_write ?? 0); return input ? `${Math.round((100 * (amounts.cache_read ?? 0)) / input)}%` : "-"; } };

const sum = (amounts) => Object.values(amounts).reduce((total, amount) => total + (amount ?? 0), 0);

const partCost = (days, chart, part) => days.reduce((total, day) => total + (chart.of(day)[part] ?? 0), 0);

// A row's bar: the parts in their fixed order, as wide as `share` of the track.
const Bar = ({ chart, parts, share }) => html`
  <span class="usage-track"><span class="usage-bar" style=${{ width: `${share}%` }}>
    ${chart.parts.filter(([part]) => parts[part]).map(([part, label]) => html`<i key=${part} data-part=${part} title=${`${label} ${chart.format(parts[part])}`} style=${{ flexGrow: parts[part] }}></i>`)}
  </span></span>`;

// The period at once, each part's amount and share in the legend, then one row per day, newest
// first, on one scale. A tap on a day shows its exact amounts (docs/web-client-tasks.md "Usage").
// The token chart's rows end with the cache hit rate: a small kind is never narrower than 4 px.
function DayRows({ days, chart, label }) {
  const [open, setOpen] = useState(null);
  const totals = days.map((day) => sum(chart.of(day)));
  const most = Math.max(1, ...totals);
  const parts = Object.fromEntries(chart.parts.map(([part]) => [part, partCost(days, chart, part)]));
  const total = sum(parts);
  const rate = (amounts) => chart.extra && html`<span class="usage-rate" title="Cache hit rate">${chart.extra(amounts)}</span>`;
  return html`
    <section class="usage-chart" data-chart=${chart.name} aria-label=${chart.title}>
      <div class="usage-total">
        <span class="usage-date">${label}</span>
        <${Bar} chart=${chart} parts=${parts} share=${total ? 100 : 0} />
        <span class="usage-cost">${chart.format(total)}</span>
        ${rate(parts)}
      </div>
      <ul class="usage-legend" aria-label=${`${chart.legend}, ${label}`}>
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

// The period choices; a limit window's only while Claude reports one (`usage::limits`).
const PERIODS = [["today", "Today"], ["week", "7 days"], ["five_hour", "5-hour window"], ["seven_day", "7-day window"]];
const WINDOWS = ["five_hour", "seven_day"];
const WHO = [["", "Workers and manager"], ["workers", "Workers"], ["manager", "Manager"]];
const ADAPTERS = { claude: "Claude", pi: "Pi", unknown: "Unknown" };

// The query of one choice of filters; Rust tells when the period starts (`usage::start`).
const query = ({ period, who, agent }) => new URLSearchParams(Object.entries({ period, who, agent }).filter(([, value]) => value)).toString();

// The agent choice: All, then per adapter all of it and each model (`usage.rs` `agents`).
function AgentSelect({ agents, value, onChange }) {
  const adapters = agents.filter((agent) => !agent.includes("/"));
  return html`
    <label class="usage-filter"><span>Agent</span>
      <select name="agent" value=${value} onChange=${(event) => onChange(event.target.value)}>
        <option value="">All agents</option>
        ${adapters.map((adapter) => adapter === "unknown" ? html`<option key=${adapter} value=${adapter}>Unknown</option>` : html`
          <optgroup key=${adapter} label=${ADAPTERS[adapter] ?? adapter}>
            <option value=${adapter}>All ${ADAPTERS[adapter] ?? adapter}</option>
            ${agents.filter((agent) => agent.startsWith(`${adapter}/`)).map((agent) => html`<option key=${agent} value=${agent}>${agent.slice(adapter.length + 1)}</option>`)}
          </optgroup>`)}
      </select>
    </label>`;
}

// Claude's limit state: each window's used share, status and reset (docs/web-client-tasks.md "All projects").
function Limits({ limits }) {
  const at = (ms) => new Date(ms).toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit" });
  const names = { five_hour: "5 hours", seven_day: "7 days" };
  const tone = (status) => status === "rejected" ? "danger" : status === "warning" ? "attention" : null;
  return limits.map((limit) => html`
    <h2 key=${`${limit.agent}-label`} class="label usage-projects-label">${ADAPTERS[limit.agent] ?? limit.agent} limit</h2>
    <div key=${limit.agent} class="table-wrap"><table aria-label=${`${ADAPTERS[limit.agent] ?? limit.agent} limit`}>
      <thead><tr><th scope="col">Window</th><th scope="col" data-align="right">Used</th><th scope="col">Status</th><th scope="col" data-align="right">Resets</th></tr></thead>
      <tbody>${limit.windows.map((window) => html`
        <tr key=${window.kind}><th scope="row">${names[window.kind] ?? window.kind.replaceAll("_", " ")}</th>
          <td data-align="right">${window.utilization == null ? "-" : `${Math.round(window.utilization * 100)}%`}</td>
          <td data-tone=${tone(window.status)}>${window.status || "-"}</td>
          <td data-align="right">${at(window.resets_ms)}</td></tr>`)}</tbody>
    </table></div>
    <p key=${`${limit.agent}-at`} class="note usage-as-of">As of ${new Date(limit.as_of_ms).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })}</p>`);
}

// One set of filters over the chart and the totals of one spend: a project's
// (`api/tasks/<project>/usage`) or every granted project's (`api/tasks/usage`, with `all`:
// the limit state and the cost per project). It reads again on a filter change and after each
// `tasks` event (docs/web-client-tasks.md "Usage").
function UsageView({ url, all = false }) {
  const { tasks } = useStore();
  const [filters, setFilters] = useState({ period: "week", who: "", agent: "" });
  // ^ One chart at a time, Cost or Tokens, so each day shows once.
  const [chart, setChart] = useState(COST);
  // ^ The newest read and the query it answered: the filters are busy until it is this one's.
  const [{ usage, loaded }, setRead] = useState({ usage: null, loaded: null });
  const [error, setError] = useState("");
  const windows = (usage?.limits ?? []).flatMap((limit) => limit.windows).filter((each) => each.start_ms);
  // ^ A limit window counts Claude's turns: absent with another agent or no known window.
  const claude = !filters.agent || filters.agent === "claude" || filters.agent.startsWith("claude/");
  const periods = PERIODS.filter(([period]) => !WINDOWS.includes(period) || (claude && windows.some((each) => each.kind === period)));
  const period = periods.some(([each]) => each === filters.period) ? filters.period : "week";
  const search = query({ ...filters, period: period === "week" ? "" : period });
  useEffect(() => {
    let live = true;
    api(search ? `${url}?${search}` : url).then((value) => { if (live) { setRead({ usage: value, loaded: search }); setError(""); } }, (failure) => { if (live) setError(failure.message); });
    return () => { live = false; };
  }, [url, search, tasks]);
  if (!usage) return error ? html`<p class="error" role="alert">${error}</p>` : html`<p class="empty">Loading usage</p>`;
  const choose = (change) => setFilters({ ...filters, period, ...change });
  const label = PERIODS.find(([each]) => each === period)[1];
  const total = usage.total ?? {};
  return html`
    ${error && html`<p class="error" role="alert">${error}</p>`}
    <div class="usage-filters" aria-busy=${loaded !== search}>
      <label class="usage-filter"><span>Period</span>
        <select name="period" value=${period} onChange=${(event) => choose({ period: event.target.value, agent: WINDOWS.includes(event.target.value) && !filters.agent ? "claude" : filters.agent })}>
          ${periods.map(([value, name]) => html`<option key=${value} value=${value}>${name}</option>`)}
        </select>
      </label>
      <label class="usage-filter"><span>Who</span>
        <select name="who" value=${filters.who} onChange=${(event) => choose({ who: event.target.value })}>
          ${WHO.map(([value, name]) => html`<option key=${value} value=${value}>${name}</option>`)}
        </select>
      </label>
      <${AgentSelect} agents=${usage.agents ?? []} value=${filters.agent} onChange=${(agent) => choose({ agent })} />
    </div>
    <div class="usage-view" role="tablist" aria-label="Chart">
      ${[COST, TOKENS].map((each) => html`<button key=${each.name} type="button" role="tab" aria-selected=${chart === each} onClick=${() => setChart(each)}>${each.title}</button>`)}
    </div>
    <${DayRows} key=${chart.name} days=${usage.days ?? []} chart=${chart} label=${label}/>
    <div class="table-wrap usage-tokens">
      <table aria-label="Token use and cost">
        <thead><tr><td></td><th scope="col" data-align="right">${label}</th></tr></thead>
        <tbody>
          ${ROWS.map(([name, value]) => html`
            <tr key=${name}><th scope="row">${name}</th><td data-align="right">${value(total)}</td></tr>`)}
        </tbody>
      </table>
    </div>
    ${all && html`<${Limits} limits=${usage.limits ?? []} />`}
    ${all && usage.projects?.length > 0 && html`
      <h2 class="label usage-projects-label">Projects</h2>
      <div class="table-wrap"><table aria-label="Cost per project">
        <thead><tr><th scope="col">Project</th><th scope="col" data-align="right">${label}</th></tr></thead>
        <tbody>${usage.projects.map((row) => html`
          <tr key=${row.name}><th scope="row">${row.name}</th><td data-align="right">${usd(row.cost_micro_usd)}</td></tr>`)}</tbody>
      </table></div>`}`;
}

export function UsagePage({ project, onClose, grip }) {
  useEffect(() => { grip.ready(); }, []);
  return html`
    <div class="page task-page">
      <${SheetBar} grip=${grip} onClose=${onClose}><span class="bar-label">Usage</span></${SheetBar}>
      <div class="scroll usage"><${UsageView} url=${`api/tasks/${project}/usage`} /></div>
    </div>`;
}

// Settings > Usage: every project this device may see, summed in Rust (`usage::all`), the
// same filters and charts as a project's sheet, Claude's limit state, and the cost per
// project in the period, the dearest first (docs/web-client-tasks.md "All projects").
export function AllUsage() {
  return html`<div class="usage" data-scope="all"><${UsageView} url="api/tasks/usage" all /></div>`;
}
