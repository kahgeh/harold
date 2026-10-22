// Attaches one photo to a reply. The photo button opens the phone's own chooser; the chosen
// photo is drawn to a canvas no longer than 2,048 px on its longer side and sent to Harold
// as JPEG at quality 0.85, which also turns an iPhone's HEIC photo into JPEG. The upload
// goes at once, with fetch rather than Datastar, because Datastar cannot send a raw image:
// Harold answers the photo's id as plain text, which goes into the hidden input bound to
// the `photo` signal, so the reply's own post names it.
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

  async function upload(blob) {
    const response = await fetch("/replies/photo", {
      method: "POST",
      headers: { "Content-Type": "application/octet-stream" },
      body: blob,
    });
    if (!response.ok) {
      throw new Error("upload refused");
    }
    return (await response.text()).trim();
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
