// Settings: this device's notification consent and its pairing.
import { api } from "./api.js";
import { add, h } from "./dom.js";

const TOPICS = [
  ["blocked", "An agent needs my input"],
  ["done", "An agent finished"],
  ["error", "An agent stopped with an error"],
];

function keyBytes(base64url) {
  const text = atob(base64url.replaceAll("-", "+").replaceAll("_", "/"));
  return Uint8Array.from(text, (character) => character.charCodeAt(0));
}

export function settingsView(app, main) {
  const status = h("p", { class: "small", role: "status" });
  const boxes = TOPICS.map(([topic, label]) => [topic, h("input", { type: "checkbox", value: topic }), label]);
  const save = h("button", { class: "primary", type: "button", onclick: apply }, "Save notification choices");
  const fieldset = h(
    "fieldset",
    {},
    h("legend", {}, "Notify this device when"),
    boxes.map(([, box, label]) => h("label", {}, box, label)),
    h("div", { class: "row" }, save),
    status,
  );
  let publicKey = null;

  function say(text, failed) {
    status.className = failed ? "small error" : "small muted";
    status.textContent = text;
  }

  async function load() {
    if (!("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window)) {
      save.disabled = true;
      say("This browser cannot receive notifications here. On iPhone, add the site to the Home Screen first.");
      return;
    }
    try {
      const data = await api("api/push");
      publicKey = data.public_key;
      for (const [topic, box] of boxes) box.checked = data.topics.includes(topic);
    } catch (failure) {
      save.disabled = true;
      say(failure.message, true);
    }
  }

  // Runs only from the button, so the permission prompt follows an explicit choice.
  async function apply() {
    const topics = boxes.filter(([, box]) => box.checked).map(([topic]) => topic);
    save.disabled = true;
    try {
      const registration = await navigator.serviceWorker.ready;
      if (!topics.length) {
        await (await registration.pushManager.getSubscription())?.unsubscribe();
        await api("api/push", { method: "DELETE", json: {} });
        say("Notifications are off for this device.");
      } else {
        if ((await Notification.requestPermission()) !== "granted") {
          throw new Error("The browser did not allow notifications for this site.");
        }
        const subscription =
          (await registration.pushManager.getSubscription()) ??
          (await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(publicKey) }));
        const { endpoint, keys, expirationTime } = subscription.toJSON();
        await api("api/push", { method: "POST", json: { subscription: { endpoint, keys, expirationTime }, topics } });
        say("Saved. This device will be notified.");
      }
    } catch (failure) {
      say(failure.message, true);
    }
    save.disabled = false;
  }

  const unpair = h("button", {
    class: "danger", type: "button",
    onclick: async () => {
      if (!confirm("Unpair this device? You will need a new code to use this site again.")) return;
      try {
        await api("api/logout", { method: "POST", json: {} });
        location.reload();
      } catch (failure) {
        say(failure.message, true);
      }
    },
  }, "Unpair this device");

  add(main,
    h("h2", {}, "Settings"),
    app.can("push") ? fieldset : null,
    h("p", {}, `Paired as "${app.device.name}" (device ${app.device.id}).`),
    h("div", { class: "row" }, unpair),
  );
  if (app.can("push")) load();
  return {};
}
