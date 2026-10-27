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

export async function api(path, { method = "GET", json, body, query } = {}) {
  const url = new URL(path, document.baseURI);
  for (const [key, value] of Object.entries(query ?? {})) url.searchParams.set(key, value);
  const init = { method, credentials: "same-origin", headers: {} };
  if (json !== undefined) {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(json);
  } else if (body !== undefined) {
    init.body = body;
  }
  let response;
  try {
    response = await fetch(url, init);
  } catch {
    throw new ApiError(0, "offline", "Cannot reach the server. Check the connection.");
  }
  const data = await response.json().catch(() => null);
  if (!response.ok) {
    const error = data?.error ?? {};
    throw new ApiError(response.status, error.code ?? "http_error", error.message ?? `Request failed (${response.status}).`);
  }
  return data;
}

// handlers: { workspace, history, exchange, daemon, resync, revoked, tasks, task, manager, down, up }
// ^ This page's id for its event stream and its presence (docs/web-client-tasks.md "Task notifications").
export const pageId = crypto.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`;

// Returns `resume`, for a page back from the background or the network: a closed stream opens
// again, and an open one may have missed events while the page slept, so `up` runs again
// (docs/web-client.md "Event stream").
export function openEvents(handlers) {
  let stream, retry, retries = 0;
  const open = () => {
    clearTimeout(retry);
    stream = new EventSource(resolve(`api/events?page=${encodeURIComponent(pageId)}`));
    for (const name of ["workspace", "history", "exchange", "daemon", "resync", "revoked", "tasks", "task", "manager", "page"]) {
      stream.addEventListener(name, (event) => handlers[name]?.(JSON.parse(event.data)));
    }
    stream.onopen = () => {
      retries = 0;
      handlers.up?.();
    };
    stream.onerror = () => {
      handlers.down?.();
      // ^ The page opens the stream again itself, soon: a deploy restarts the server in about
      // 3 s, and EventSource waits its own delay or, after an HTTP error, gives up.
      stream.close();
      retry = setTimeout(open, RETRY_MS[Math.min(retries++, RETRY_MS.length - 1)]);
    };
  };
  open();
  return () => {
    if (stream.readyState === 2) open();
    // ^ A stream that is connecting calls `up` once it opens.
    else if (stream.readyState === 1) handlers.up?.();
  };
}
