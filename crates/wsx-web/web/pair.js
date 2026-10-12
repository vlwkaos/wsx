// Pairing: the host owner runs `wsx web pair --site <file>` and reads out the code.
import { api } from "./api.js";
import { html, useRef, useState } from "./lib.js";

export function Pair({ onPaired }) {
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const code = useRef();
  const name = useRef();
  const submit = async (event) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api("api/pair", { method: "POST", json: { code: code.current.value, name: name.current.value } });
      onPaired();
    } catch (failure) {
      // The typed code stays in the field so a typo can be fixed.
      setError(failure.message);
      setBusy(false);
      code.current.focus();
    }
  };
  return html`
    <main class="screen pair" id="main">
      <form class="pair-form" onSubmit=${submit}>
        <h1 class="title">Pair this device</h1>
        <p class="muted">Ask the owner for a one-time code. It works once and expires after 10 minutes.</p>
        <label class="field">Pairing code
          <input class="input" type="text" id="pair-code" name="code" ref=${code} autocomplete="one-time-code"
            autocapitalize="characters" spellcheck=${false} required maxlength="16" autofocus />
        </label>
        <label class="field">Device name (optional)
          <input class="input" type="text" id="pair-name" name="name" ref=${name} maxlength="40" placeholder="for example: my phone" />
        </label>
        <button class="button button-primary" type="submit" disabled=${busy}>Pair this device</button>
        <p class="error" role="alert">${error}</p>
      </form>
    </main>`;
}
