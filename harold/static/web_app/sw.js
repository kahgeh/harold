// Harold's service worker: shows the chief's alerts, and opens the app when one is tapped.
"use strict";

self.addEventListener("push", (event) => {
  const body = event.data ? event.data.text() : "";
  event.waitUntil(self.registration.showNotification("Chief", { body }));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((windows) => {
      for (const client of windows) {
        if (new URL(client.url).pathname === "/" && "focus" in client) {
          return client.focus();
        }
      }
      return self.clients.openWindow("/");
    }),
  );
});
