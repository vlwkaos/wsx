// Settings: this device's appearance, its notification consent and its pairing.
import { api } from "./api.js";
import { html, useEffect, useState } from "./lib.js";
import { can, state } from "./store.js";
import { confirmDialog, hasDarkScheme, scheme, setScheme } from "./ui.js";

const TOPICS = [
  ["blocked", "An agent needs my input"],
  ["done", "An agent finished"],
  ["error", "An agent stopped with an error"],
];

function keyBytes(base64url) {
  const text = atob(base64url.replaceAll("-", "+").replaceAll("_", "/"));
  return Uint8Array.from(text, (character) => character.charCodeAt(0));
}

function Push() {
  const [topics, setTopics] = useState([]);
  const [key, setKey] = useState(null);
  const [status, setStatus] = useState({ text: "" });
  const [busy, setBusy] = useState(true);
  useEffect(() => {
    if (!("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window)) {
      setStatus({ text: "This browser cannot receive notifications here. On iPhone, add the site to the Home Screen first." });
      return;
    }
    api("api/push").then((data) => { setKey(data.public_key); setTopics(data.topics); setBusy(false); })
      .catch((failure) => setStatus({ text: failure.message, failed: true }));
  }, []);
  // Runs only from the button, so the permission prompt follows an explicit choice.
  const apply = async () => {
    setBusy(true);
    try {
      const registration = await navigator.serviceWorker.ready;
      if (!topics.length) {
        await (await registration.pushManager.getSubscription())?.unsubscribe();
        await api("api/push", { method: "DELETE", json: {} });
        setStatus({ text: "Notifications are off for this device." });
      } else {
        if ((await Notification.requestPermission()) !== "granted") throw new Error("The browser did not allow notifications for this site.");
        const subscription = (await registration.pushManager.getSubscription())
          ?? (await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(key) }));
        const { endpoint, keys, expirationTime } = subscription.toJSON();
        await api("api/push", { method: "POST", json: { subscription: { endpoint, keys, expirationTime }, topics } });
        setStatus({ text: "Saved. This device will be notified." });
      }
    } catch (failure) {
      setStatus({ text: failure.message, failed: true });
    }
    setBusy(false);
  };
  const toggle = (topic) => setTopics((list) => list.includes(topic) ? list.filter((other) => other !== topic) : [...list, topic]);
  return html`
    <section aria-label="Notifications">
      <h2 class="label">Notify this device when</h2>
      <div class="options">
        ${TOPICS.map(([topic, label]) => html`
          <label class="option" key=${topic}><input type="checkbox" checked=${topics.includes(topic)} onChange=${() => toggle(topic)} />${label}</label>`)}
      </div>
      <div class="actions">
        <button class="button" type="button" disabled=${busy} onClick=${apply}>Save notification choices</button>
        <span class=${status.failed ? "error" : "muted"} role="status">${status.text}</span>
      </div>
    </section>`;
}

export function Settings() {
  const [current, setCurrent] = useState(scheme());
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
    <div class="page">
      <h1 class="title">Settings</h1>
      <div class="scroll settings">
        ${hasDarkScheme() && html`
          <section aria-label="Appearance">
            <h2 class="label">Appearance on this device</h2>
            <div class="options">
              ${[["dark", "Dark"], ["light", "Light"]].map(([value, label]) => html`
                <label class="option" key=${value}>
                  <input type="radio" name="scheme" value=${value} checked=${current === value}
                    onChange=${() => { setScheme(value); setCurrent(value); }} />${label}</label>`)}
            </div>
          </section>`}
        ${can("push") && html`<${Push} />`}
        <section aria-label="This device">
          <h2 class="label">This device</h2>
          <p class="muted device">Paired as "${state.device.name}" (device ${state.device.id}).</p>
          <div class="actions">
            <button class="button button-danger" type="button" onClick=${unpair}>Unpair this device</button>
            ${error && html`<span class="error" role="alert">${error}</span>`}
          </div>
        </section>
      </div>
    </div>`;
}
