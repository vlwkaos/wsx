// The one fetch wrapper and the one event stream. Every URL is relative to
// <base href>, so the same files work at a sub-path and at an alt-port root.

class ApiError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

// How soon a lost stream or terminal opens again. A lost connection is said only after
// QUIET_MS: a deploy restarts the server in about 3 s, and an open page shows nothing for it.
export const RETRY_MS = [250, 500, 1000, 2000, 5000];
export const QUIET_MS = 5000;

export const resolve = (path) => new URL(path, document.baseURI).href;

// `timeout`: a request on a connection that went silent (a phone back from sleep) would
// wait for minutes, and a chat waits for its read; a read gives up after READ_MS.
const READ_MS = 20_000;
export async function api(path, { method = "GET", json, body, query, timeout = method === "GET" ? READ_MS : 0 } = {}) {
  const url = new URL(path, document.baseURI);
  for (const [key, value] of Object.entries(query ?? {})) url.searchParams.set(key, value);
  const init = { method, credentials: "same-origin", headers: {} };
  if (json !== undefined) {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(json);
  } else if (body !== undefined) {
    init.body = body;
  }
  if (timeout) init.signal = AbortSignal.timeout(timeout);
  let response, text;
  try {
    response = await fetch(url, init);
    text = await response.text();
  } catch (error) {
    if (error?.name === "TimeoutError") throw new ApiError(0, "timeout", "The server did not answer in time. Try again.");
    throw new ApiError(0, "offline", "Cannot reach the server. Check the connection.");
  }
  let data = null;
  try { data = JSON.parse(text); } catch { /* not JSON: the status says what happened */ }
  if (!response.ok) {
    const error = data?.error ?? {};
    throw new ApiError(response.status, error.code ?? "http_error", error.message ?? `Request failed (${response.status}).`);
  }
  return data;
}

// handlers: { workspace, history, exchange, daemon, resync, revoked, tasks, task, manager, page, down, up }
// ^ This page's id for its event stream and its presence (docs/web-client-tasks.md "Task notifications").
export const pageId = crypto.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`;

// ^ The server sends `ping` every 10 s on a quiet stream. A stream that says nothing for
// SILENT_MS is dead without an error (a network change, a phone back from sleep): it opens again.
const SILENT_MS = 25_000;

// Returns `resume`, for a page back from the background or the network: its stream may look
// open and be dead, so it opens again, and `up` reads again what the events would have changed
// (docs/web-client.md "Event stream").
export function openEvents(handlers) {
  let stream, retry, silent, retries = 0;
  const heard = () => {
    clearTimeout(silent);
    silent = setTimeout(lost, SILENT_MS);
  };
  const lost = () => {
    handlers.down?.();
    // ^ The page opens the stream again itself, soon: a deploy restarts the server in about
    // 3 s, and EventSource waits its own delay or, after an HTTP error, gives up.
    stream.close();
    clearTimeout(silent);
    retry = setTimeout(open, RETRY_MS[Math.min(retries++, RETRY_MS.length - 1)]);
  };
  const open = () => {
    clearTimeout(retry);
    stream?.close();
    stream = new EventSource(resolve(`api/events?page=${encodeURIComponent(pageId)}`));
    heard();
    stream.addEventListener("ping", heard);
    for (const name of ["workspace", "history", "exchange", "daemon", "resync", "revoked", "tasks", "task", "manager", "page"]) {
      stream.addEventListener(name, (event) => {
        heard();
        handlers[name]?.(JSON.parse(event.data));
      });
    }
    stream.onopen = () => {
      retries = 0;
      heard();
      handlers.up?.();
    };
    stream.onerror = lost;
  };
  open();
  return open;
}
