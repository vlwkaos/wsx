// The one fetch wrapper and the one event stream. Every URL is relative to
// <base href>, so the same files work at a sub-path and at an alt-port root.

export class ApiError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

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
export function openEvents(handlers) {
  const stream = new EventSource(resolve("api/events"));
  for (const name of ["workspace", "history", "exchange", "daemon", "resync", "revoked", "tasks", "task", "manager"]) {
    stream.addEventListener(name, (event) => handlers[name]?.(JSON.parse(event.data)));
  }
  stream.onopen = () => handlers.up?.();
  stream.onerror = () => {
    handlers.down?.();
    // EventSource reconnects by itself after a dropped connection. After an HTTP
    // error it gives up (readyState 2), so open a new stream once, a little later.
    if (stream.readyState === 2) setTimeout(() => openEvents(handlers), 5000);
  };
  return stream;
}
