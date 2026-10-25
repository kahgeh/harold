// Turns on alerts: asks for permission, registers the service worker, subscribes to
// pushes with Harold's VAPID key, and sends the subscription to Harold. On load, a
// subscription the browser already holds is sent again, so the button hides only when
// Harold holds it too.
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

  // Harold's public key, from the page, as bytes.
  function pageKey() {
    const meta = document.querySelector('meta[name="harold-vapid-key"]');
    return decodeBase64url(meta.content);
  }

  // True when the subscription was made with the page's key, byte for byte.
  function madeWithPageKey(subscription) {
    const held = subscription.options && subscription.options.applicationServerKey;
    if (!held) {
      return false;
    }
    const heldBytes = new Uint8Array(held);
    const keyBytes = pageKey();
    return (
      heldBytes.length === keyBytes.length &&
      heldBytes.every((byte, i) => byte === keyBytes[i])
    );
  }

  // Datastar's own module, the one the page loads: its signals and its @post.
  const datastar = import("/static/datastar-1.0.2.js");

  // Sets `signals`, then sends `include`d signals with Datastar's @post (or `action`). Resolves
  // to the HTTP status when Datastar announced an error for the request, else 200; rejects when
  // the request could not be sent. A request is never retried, and it stays open while the page hides (the
  // default would abort it and send it again on return), so one failed post is one
  // post, and a 401 (the device is no longer paired) reloads to the pair screen.
  async function post(element, url, signals, include, options = {}) {
    // A request with no element of its own gets one, so another request's error is not taken for its.
    const el = element || document.createElement("span");
    const { actions, mergePatch } = await datastar;
    mergePatch(signals);
    let status = 200;
    const note = (event) => {
      const detail = event.detail;
      if (detail && detail.type === "error" && detail.el === el) {
        status = Number(detail.argsRaw && detail.argsRaw.status) || 0;
      }
    };
    document.addEventListener("datastar-fetch", note);
    try {
      await actions[options.action || "post"](
        { el, evt: undefined, error: (name) => new Error(name), cleanups: new Map() },
        url,
        {
          filterSignals: { include },
          openWhenHidden: true,
          requestCancellation: "disabled",
          retry: "never",
          retryMaxCount: 0,
          ...options.request,
        },
      );
    } finally {
      document.removeEventListener("datastar-fetch", note);
    }
    if (status === 401) {
      location.reload();
    }
    return status;
  }

  // Sends the subscription to Harold, which stores it by its address. True when Harold
  // stored it.
  async function send(subscription) {
    try {
      // The signals stay in the page's store after the post, so they are cleared after it.
      const status = await post(
        button,
        "/push/subscription",
        {
          endpoint: subscription.endpoint,
          keys: {
            p256dh: encodeBase64url(subscription.getKey("p256dh")),
            auth: encodeBase64url(subscription.getKey("auth")),
          },
        },
        /^(endpoint|keys\..*)$/,
      ).finally(async () => (await datastar).mergePatch({ endpoint: null, keys: null }));
      return status === 200;
    } catch {
      // Harold could not be reached: the subscription is not stored.
      return false;
    }
  }

  async function dropQuietly(subscription) {
    try {
      await subscription.unsubscribe();
    } catch {
      // Nothing more to do; the button stays.
    }
  }

  async function turnOn() {
    button.disabled = true;
    try {
      const permission = await Notification.requestPermission();
      if (permission !== "granted") {
        // The button stays, for another tap.
        return;
      }
      const registration = await navigator.serviceWorker.register("/static/sw.js", {
        scope: "/",
      });
      await navigator.serviceWorker.ready;
      // A subscription still held here is one Harold did not store, or one made with
      // another key: drop it and subscribe again.
      const held = await registration.pushManager.getSubscription();
      if (held) {
        await dropQuietly(held);
      }
      const subscription = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: pageKey(),
      });
      if (await send(subscription)) {
        hide();
        return;
      }
      // Harold did not store it: drop the browser's copy, so the next load does not
      // take it for a working sign-up, and leave the button for another tap.
      await dropQuietly(subscription);
    } finally {
      button.disabled = false;
    }
  }

  // On load: a subscription made with another key is dropped, and one made with the
  // page's key is sent again; the button hides only when Harold stored it.
  async function checkHeld() {
    const registration = await navigator.serviceWorker.getRegistration("/");
    const subscription = registration && (await registration.pushManager.getSubscription());
    if (!subscription) {
      return;
    }
    if (!madeWithPageKey(subscription)) {
      await dropQuietly(subscription);
      return;
    }
    if (await send(subscription)) {
      hide();
    }
  }

  if (!("serviceWorker" in navigator) || !("PushManager" in window)) {
    hide();
    return;
  }

  checkHeld().catch(() => {});

  button.addEventListener("click", () => {
    turnOn().catch(() => {});
  });
})();
