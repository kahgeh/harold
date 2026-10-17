// Turns on alerts: asks for permission, registers the service worker, subscribes to
// pushes with Harold's VAPID key, and sends the subscription to Harold.
"use strict";

(function () {
  const button = document.getElementById("alerts");
  if (!button) {
    return;
  }

  function hide() {
    button.hidden = true;
  }

  // The key in the page is base64url without padding; subscribe wants its bytes.
  function decodeBase64url(text) {
    const base64 = text.replace(/-/g, "+").replace(/_/g, "/");
    const padded = base64 + "=".repeat((4 - (base64.length % 4)) % 4);
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) {
      bytes[i] = binary.charCodeAt(i);
    }
    return bytes;
  }

  function encodeBase64url(buffer) {
    let binary = "";
    for (const byte of new Uint8Array(buffer)) {
      binary += String.fromCharCode(byte);
    }
    return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  async function turnOn() {
    button.disabled = true;
    try {
      const permission = await Notification.requestPermission();
      if (permission !== "granted") {
        return;
      }
      const registration = await navigator.serviceWorker.register("/static/sw.js", {
        scope: "/",
      });
      await navigator.serviceWorker.ready;
      const meta = document.querySelector('meta[name="harold-vapid-key"]');
      const applicationServerKey = decodeBase64url(meta.content);
      const subscription = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey,
      });
      const response = await fetch("/push/subscription", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          endpoint: subscription.endpoint,
          keys: {
            p256dh: encodeBase64url(subscription.getKey("p256dh")),
            auth: encodeBase64url(subscription.getKey("auth")),
          },
        }),
      });
      if (response.ok) {
        hide();
      }
    } finally {
      button.disabled = false;
    }
  }

  if (!("serviceWorker" in navigator) || !("PushManager" in window)) {
    hide();
    return;
  }

  navigator.serviceWorker
    .getRegistration("/")
    .then((registration) => registration && registration.pushManager.getSubscription())
    .then((subscription) => {
      if (subscription) {
        hide();
      }
    })
    .catch(() => {});

  button.addEventListener("click", () => {
    turnOn().catch(() => {});
  });
})();
