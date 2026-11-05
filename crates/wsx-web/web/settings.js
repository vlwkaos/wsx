// Settings: General (notifications, version, pairing), UI (appearance, header, bottom bar) and Usage.
// Every setting here is kept per device.
import { api } from "./api.js";
import { html, useEffect, useState } from "./lib.js";
import { can, loadPush, pageBuild, reloadForBuild, replaceRoute, setMuted, state, tabs, update, useStore } from "./store.js";
import { confirmDialog, hasDarkScheme, scheme, setBar, setScheme, setTabHidden } from "./ui.js";
import { AllUsage } from "./usage.js";

// The two switches of this device (docs/web-client.md "Notifications").
const SWITCHES = [
  ["manager", "Manager needs me", "Tasks, approvals, things to check", "task_view"],
  ["agents", "An agent needs me", "Input needed or an error"],
];

function keyBytes(base64url) {
  const text = atob(base64url.replaceAll("-", "+").replaceAll("_", "/"));
  return Uint8Array.from(text, (character) => character.charCodeAt(0));
}

// Each change saves at once; the permission prompt follows a switch the owner turned on.
// A failed save leaves the switches as stored and keeps its error until the next save.
function Push() {
  const store = useStore();
  const [status, setStatus] = useState({ text: "" });
  const [busy, setBusy] = useState(true);
  const supported = "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;
  useEffect(() => {
    if (!supported) {
      setStatus({ text: "This browser cannot receive notifications here. On iPhone, add the site to the Home Screen first.", failed: true });
      return;
    }
    loadPush().then(() => setBusy(false), (failure) => setStatus({ text: failure.message, failed: true }));
  }, []);
  const choices = store.push?.choices ?? { manager: false, agents: false };
  const muted = store.push?.muted ?? [];
  const save = async (next) => {
    setBusy(true);
    try {
      const registration = await navigator.serviceWorker.ready;
      if (!next.manager && !next.agents) {
        await (await registration.pushManager.getSubscription())?.unsubscribe();
        update({ push: await api("api/push", { method: "DELETE", json: {} }) });
        setStatus({ text: "Notifications are off for this device." });
      } else {
        const permission = await Notification.requestPermission();
        if (permission === "denied") throw new Error("The browser blocks notifications for this site. Allow them in the browser's site settings, then turn the switch on again.");
        if (permission !== "granted") throw new Error("The browser did not allow notifications for this site.");
        const subscription = (await registration.pushManager.getSubscription())
          ?? (await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(store.push.public_key) }));
        const { endpoint, keys, expirationTime } = subscription.toJSON();
        update({ push: await api("api/push", { method: "POST", json: { subscription: { endpoint, keys, expirationTime }, choices: next } }) });
        setStatus({ text: "Saved." });
      }
    } catch (failure) {
      setStatus({ text: failure.message, failed: true });
    }
    setBusy(false);
  };
  const flip = (name) => save({ ...choices, [name]: !choices[name] });
  const unmute = async () => {
    setBusy(true);
    try {
      for (const session of muted) await setMuted(session, false);
      setStatus({ text: "Every session is unmuted." });
    } catch (failure) {
      setStatus({ text: failure.message, failed: true });
    }
    setBusy(false);
  };
  return html`
    <section aria-label="Notifications">
      <h2 class="label">Notifications</h2>
      <div class="options">
        ${SWITCHES.filter(([, , , needs]) => !needs || can(needs)).map(([name, label, hint]) => html`
          <label class="option option-row" key=${name}>
            <span class="option-text">${label}<span class="option-hint">${hint}</span></span>
            <input type="checkbox" role="switch" name=${name} checked=${choices[name]}
              disabled=${busy} onChange=${() => flip(name)} />
          </label>`)}
        ${muted.length > 0 && html`
          <div class="option option-row">
            <span class="option-text">${muted.length} ${muted.length === 1 ? "session" : "sessions"} muted</span>
            <button class="button" type="button" disabled=${busy} onClick=${unmute}>Unmute all</button>
          </div>`}
      </div>
      ${status.text && html`
        <p class=${`option-status ${status.failed ? "error" : "muted"}`} role=${status.failed ? "alert" : "status"}>${status.text}</p>`}
    </section>`;
}

// Appearance, header position and the bottom bar's tabs, one switch each, from the one list the bar reads (store.js `tabs`).
// Settings is always shown, so the way back to these switches stays (docs/web-client.md "Settings").
function Interface() {
  const store = useStore();
  const [current, setCurrent] = useState(scheme());
  return html`
    ${hasDarkScheme() && html`
      <section aria-label="Appearance">
        <h2 class="label">Appearance</h2>
        <div class="options">
          ${[["dark", "Dark"], ["light", "Light"]].map(([value, label]) => html`
            <label class="option" key=${value}>
              <input type="radio" name="scheme" value=${value} checked=${current === value}
                onChange=${() => { setScheme(value); setCurrent(value); }} />${label}</label>`)}
        </div>
      </section>`}
    <section aria-label="Header">
      <h2 class="label">Header</h2>
      <div class="options">
        ${[["top", "Top"], ["bottom", "Bottom"]].map(([value, label]) => html`
          <label class="option" key=${value}>
            <input type="radio" name="bar" value=${value} checked=${store.bar === value}
              onChange=${() => setBar(value)} />${label}</label>`)}
      </div>
    </section>
    <section aria-label="Bottom bar">
      <h2 class="label">Bottom bar</h2>
      <div class="options">
        ${tabs().map(([name, , label]) => {
          const always = name === "settings";
          const shown = always || !store.hiddenTabs.includes(name);
          return html`
            <label class="option option-row" key=${name}>
              <span class="option-text">${label}${always && html`<span class="option-hint">Always shown</span>`}</span>
              <input type="checkbox" role="switch" name=${`tab-${name}`} checked=${shown} disabled=${always}
                onChange=${() => setTabHidden(name, shown)} />
            </label>`;
        })}
      </div>
    </section>`;
}

function General() {
  const [error, setError] = useState("");
  const unpair = async () => {
    const sure = await confirmDialog({
      title: "Unpair this device?",
      text: "You will need a new code from the owner to use this site again.",
      confirm: "Unpair",
      danger: true,
    });
    if (!sure) return;
    try {
      await api("api/logout", { method: "POST", json: {} });
      location.reload();
    } catch (failure) {
      setError(failure.message);
    }
  };
  return html`
    ${can("push") && html`<${Push} />`}
    ${state.build && html`
      <section aria-label="Version">
        <h2 class="label">Version</h2>
        <dl class="facts">
          <dt>wsx</dt><dd>${state.build.version}</dd>
          <dt>Build</dt><dd><code>${state.build.commit ?? state.build.id}</code></dd>
          ${state.build.commit && html`<dt>Page files</dt><dd><code>${state.build.id}</code></dd>`}
          <dt>Server started</dt><dd>${new Date(state.build.started_unix * 1000).toLocaleString()}</dd>
        </dl>
        ${state.build.id !== pageBuild && html`
          <div class="actions">
            <span class="muted">This page is still the older version <code>${pageBuild}</code>.</span>
            <button class="button" type="button" onClick=${reloadForBuild}>Reload</button>
          </div>`}
      </section>`}
    <section aria-label="This device">
      <h2 class="label">This device</h2>
      <p class="muted device">Paired as "${state.device.name}" (device ${state.device.id}).</p>
      <div class="actions">
        <button class="button button-danger" type="button" onClick=${unpair}>Unpair this device</button>
        ${error && html`<span class="error" role="alert">${error}</span>`}
      </div>
    </section>
`;
}

// ^ #/settings/<sub>: General is the bare route. Usage needs task_view, as its route does.
const SUBS = [["general", "General"], ["ui", "UI"], ["usage", "Usage", "task_view"]];
export function Settings({ sub }) {
  const subs = SUBS.filter(([, , needs]) => !needs || can(needs));
  const current = subs.find(([name]) => name === sub)?.[0] ?? "general";
  return html`
    <div class="page">
      <h1 class="title">Settings</h1>
      <nav class="subtabs" aria-label="Settings sections">
        ${subs.map(([name, label]) => {
          const href = name === "general" ? "#/settings" : `#/settings/${name}`;
          return html`<a key=${name} href=${href} aria-current=${current === name ? "page" : null} onClick=${(event) => replaceRoute(event, href)}>${label}</a>`;
        })}
      </nav>
      <div class="scroll settings">
        ${current === "general" ? html`<${General} />` : current === "ui" ? html`<${Interface} />` : html`<${AllUsage} />`}
      </div>
    </div>`;
}
