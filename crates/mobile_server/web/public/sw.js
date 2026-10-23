// Service worker for the Zed mobile PWA: push notifications and an offline
// app shell. The precache list and version are filled in at build time by
// vite.config.ts; in development they stay empty.

const PRECACHE = /*__PRECACHE__*/ [] /*__END__*/;
const VERSION = "__BUILD_VERSION__";
const CACHE = `zed-shell-${VERSION}`;

self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      // Add individually so one missing asset doesn't fail the install.
      await Promise.all(
        ["/", ...PRECACHE].map((url) =>
          cache.add(new Request(url, { cache: "reload" })).catch((error) => console.warn("precache failed", url, error)),
        ),
      );
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const keys = await caches.keys();
      await Promise.all(keys.filter((key) => key.startsWith("zed-shell-") && key !== CACHE).map((key) => caches.delete(key)));
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // The API (including the WebSocket upgrade) always goes to the network.
  if (url.pathname.startsWith("/api/")) return;

  if (request.mode === "navigate") {
    // Launch instantly from the cached shell, refreshing it in the background.
    event.respondWith(staleWhileRevalidate(event, new Request("/"), request));
    return;
  }
  event.respondWith(staleWhileRevalidate(event, request, request));
});

async function staleWhileRevalidate(event, cacheKey, networkRequest) {
  const cache = await caches.open(CACHE);
  const cached = await cache.match(cacheKey);
  const network = fetch(networkRequest)
    .then((response) => {
      if (response.ok && response.type === "basic") cache.put(cacheKey, response.clone()).catch(() => {});
      return response;
    })
    .catch((error) => {
      if (cached) return cached;
      throw error;
    });
  if (cached) {
    event.waitUntil(network.catch(() => {}));
    return cached;
  }
  return network;
}

self.addEventListener("push", (event) => {
  let payload = { title: "Zed", body: "", tag: null, url: null };
  if (event.data) {
    try {
      payload = { ...payload, ...event.data.json() };
    } catch {
      payload.body = event.data.text();
    }
  }
  const options = {
    body: payload.body || "",
    icon: "/icons/icon-192.png",
    badge: "/icons/badge-96.png",
    data: { url: payload.url || "/" },
  };
  if (payload.tag) {
    options.tag = payload.tag;
    options.renotify = true;
  }
  event.waitUntil(self.registration.showNotification(payload.title || "Zed", options));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = (event.notification.data && event.notification.data.url) || "/";
  const absolute = new URL(target.startsWith("#") ? `/${target}` : target, self.location.origin).href;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      const client = windows.find((candidate) => new URL(candidate.url).origin === self.location.origin);
      if (client) {
        await client.focus();
        client.postMessage({ type: "navigate", url: target });
        return;
      }
      await self.clients.openWindow(absolute);
    })(),
  );
});
