// Service worker: offline shell and Web Push display. The server fills in the
// cache name and file list; every URL is relative to this file's own location.
const CACHE = "__WSX_CACHE__";
const SHELL = __WSX_SHELL__;
const scope = self.registration.scope;

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((names) => Promise.all(names.filter((name) => name !== CACHE).map((name) => caches.delete(name))))
      .then(() => self.clients.claim()),
  );
});

// Shell files: network first so an update shows at once, cache when offline.
// API and media requests are never answered from this cache.
self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || !url.href.startsWith(scope)) return;
  const path = url.href.slice(scope.length).split(/[?#]/)[0];
  if (!SHELL.includes(path === "" ? "./" : path)) return;
  const key = path === "" ? "./" : path;
  event.respondWith(
    fetch(event.request)
      .then((response) => {
        if (response.ok) {
          const copy = response.clone();
          caches.open(CACHE).then((cache) => cache.put(key, copy));
        }
        return response;
      })
      .catch(() => caches.match(key, { cacheName: CACHE })),
  );
});

self.addEventListener("push", (event) => {
  let data = {};
  try { data = event.data.json(); } catch { /* an unreadable push still must show something */ }
  event.waitUntil(
    self.registration.showNotification(data.title ?? "wsx", {
      body: data.body ?? "An agent changed state.",
      tag: data.tag,
      icon: "__WSX_ICON__",
      data: { session_id: data.session_id },
    }),
  );
});

// Open the session the notification is about, in an existing window when one is open.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const session = event.notification.data?.session_id;
  const target = session ? `${scope}#/chat/${session}` : scope;
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((windows) => {
      const open = windows.find((client) => client.url.startsWith(scope));
      return open ? open.navigate(target).then((client) => client?.focus()) : self.clients.openWindow(target);
    }),
  );
});
