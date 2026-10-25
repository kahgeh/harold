// Attaches one photo to a reply. The photo button opens the phone's own chooser; the chosen
// photo is drawn to a canvas no longer than 2,048 px on its longer side and sent to Harold
// as JPEG at quality 0.85, which also turns an iPhone's HEIC photo into JPEG. The upload
// goes at once through Datastar's @post, as the base64 `_photo` signal: the leading `_` keeps
// it out of every request by default, and only this post asks for it. Harold answers by
// patching the `photo` signal to the photo's id, which the hidden input is bound to, so the
// reply's own post names it. The script clears `_photo` once the post ends.
"use strict";

(function () {
  const form = document.getElementById("reply");
  const chooser = document.getElementById("photo-file");
  const button = document.getElementById("photo-button");
  const strip = document.getElementById("photo-chosen");
  const preview = document.getElementById("photo-preview");
  const label = document.getElementById("photo-label");
  const remove = document.getElementById("photo-remove");
  const idInput = document.getElementById("photo-id");
  if (!form || !chooser || !button || !strip || !preview || !label || !remove || !idInput) {
    return;
  }
  const send = form.querySelector('button[type="submit"]');

  const LONGEST_SIDE = 2048;
  const QUALITY = 0.85;
  const FAILED_FOR_MS = 3000;
  const THUMBNAIL = 112;

  // Counts the photos chosen, so an upload that was taken off or replaced is not used.
  let choice = 0;
  let uploading = false;
  let failedTimer = null;

  function setUploading(on) {
    uploading = on;
    if (send) {
      send.disabled = on;
    }
  }

  // Puts the photo's id in the hidden input and tells Datastar, which holds it as the
  // `photo` signal.
  function setId(id) {
    idInput.value = id;
    idInput.dispatchEvent(new Event("input", { bubbles: true }));
  }

  function size(bytes) {
    if (bytes >= 1024 * 1024) {
      return (bytes / (1024 * 1024)).toFixed(1) + " MB";
    }
    return Math.max(1, Math.round(bytes / 1024)) + " KB";
  }

  function hideStrip() {
    clearTimeout(failedTimer);
    failedTimer = null;
    strip.hidden = true;
    preview.hidden = false;
    remove.hidden = false;
    label.textContent = "";
  }

  // Takes the photo off: the strip, the id and any upload still running.
  function clear() {
    choice += 1;
    setUploading(false);
    setId("");
    chooser.value = "";
    hideStrip();
  }

  function failed() {
    setUploading(false);
    setId("");
    chooser.value = "";
    preview.hidden = true;
    remove.hidden = true;
    label.textContent = "The photo didn't reach Harold.";
    strip.hidden = false;
    clearTimeout(failedTimer);
    failedTimer = setTimeout(hideStrip, FAILED_FOR_MS);
  }

  // The photo as the browser draws it, turned upright by its orientation mark. It is
  // decoded with createImageBitmap, not an <img> on a blob: address, which the page's
  // content security policy (img-src 'self') would refuse.
  async function decode(file) {
    try {
      return await createImageBitmap(file, { imageOrientation: "from-image" });
    } catch (error) {
      return await createImageBitmap(file);
    }
  }

  // The photo as it will be sent: the canvas it is drawn to, no longer than LONGEST_SIDE.
  async function shrink(file) {
    const image = await decode(file);
    try {
      const longest = Math.max(image.width, image.height);
      const scale = Math.min(1, LONGEST_SIDE / longest);
      const canvas = document.createElement("canvas");
      canvas.width = Math.max(1, Math.round(image.width * scale));
      canvas.height = Math.max(1, Math.round(image.height * scale));
      const context = canvas.getContext("2d");
      // JPEG has no transparency: a PNG's clear parts become white, not black.
      context.fillStyle = "#fff";
      context.fillRect(0, 0, canvas.width, canvas.height);
      context.drawImage(image, 0, 0, canvas.width, canvas.height);
      return canvas;
    } finally {
      image.close();
    }
  }

  function toJpeg(canvas) {
    return new Promise((resolve) => canvas.toBlob(resolve, "image/jpeg", QUALITY));
  }

  // Draws the centre square of the photo into the strip's thumbnail.
  function showThumbnail(canvas) {
    preview.width = THUMBNAIL;
    preview.height = THUMBNAIL;
    const side = Math.min(canvas.width, canvas.height);
    const x = (canvas.width - side) / 2;
    const y = (canvas.height - side) / 2;
    preview.getContext("2d").drawImage(canvas, x, y, side, side, 0, 0, THUMBNAIL, THUMBNAIL);
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

  // The JPEG as base64, without the data: URL's front.
  function base64Of(blob) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(String(reader.result).slice(String(reader.result).indexOf(",") + 1));
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(blob);
    });
  }

  // Sends the photo, and returns the id Harold patched into the `photo` signal.
  async function upload(blob) {
    const { getPath, mergePatch } = await datastar;
    const photo = await base64Of(blob);
    try {
      // Only this post sends `_photo`: the default filter leaves it out of every other.
      const status = await post(button, "/replies/photo", { _photo: photo }, /^_photo$/, {
        request: { filterSignals: { include: /^_photo$/, exclude: /^$/ } },
      });
      if (status !== 200) {
        throw new Error("upload refused");
      }
    } finally {
      mergePatch({ _photo: "" });
    }
    const id = getPath("photo");
    if (typeof id !== "string" || id === "") {
      throw new Error("upload refused");
    }
    return id;
  }

  async function choose(file) {
    choice += 1;
    const mine = choice;
    setId("");
    clearTimeout(failedTimer);
    failedTimer = null;
    setUploading(true);
    try {
      const canvas = await shrink(file);
      const blob = await toJpeg(canvas);
      if (mine !== choice) {
        return;
      }
      if (!blob) {
        throw new Error("not shrunk");
      }
      showThumbnail(canvas);
      preview.hidden = false;
      remove.hidden = false;
      label.textContent = "Photo · " + size(blob.size);
      strip.hidden = false;
      const id = await upload(blob);
      if (mine !== choice) {
        return;
      }
      setId(id);
      setUploading(false);
    } catch (error) {
      if (mine === choice) {
        failed();
      }
    }
  }

  button.addEventListener("click", () => chooser.click());
  chooser.addEventListener("change", () => {
    const file = chooser.files && chooser.files[0];
    if (file) {
      choose(file);
    }
  });
  remove.addEventListener("click", clear);

  // A reply is not sent while its photo is still uploading.
  form.addEventListener(
    "submit",
    (event) => {
      if (uploading) {
        event.preventDefault();
        event.stopImmediatePropagation();
      }
    },
    true,
  );

  // When Harold took the reply it cleared the `photo` signal, and the strip goes with it.
  document.addEventListener("datastar-fetch", (event) => {
    const detail = event.detail;
    if (
      detail &&
      detail.type === "finished" &&
      detail.el === form &&
      idInput.value === "" &&
      !uploading &&
      !strip.hidden
    ) {
      clear();
    }
  });
})();
