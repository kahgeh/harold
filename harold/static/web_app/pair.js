// The pair screen: posts the typed code to Harold, and opens the app once it is paired.
"use strict";

(function () {
  const form = document.getElementById("pair-form");
  const code = document.getElementById("pair-code");
  const button = document.getElementById("pair-button");
  const notice = document.getElementById("pair-notice");
  if (!form || !code || !button || !notice) {
    return;
  }

  function show(text) {
    notice.textContent = text;
    notice.hidden = !text;
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    show("");
    button.disabled = true;
    try {
      const response = await fetch("/pair", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code: code.value }),
      });
      if (response.status === 204) {
        location.reload();
        return;
      }
      // Harold's own words for a refusal; nothing else is shown.
      const text = (await response.text()).trim();
      show(text && text.length < 80 ? text : "That code didn't work");
      code.value = "";
    } catch {
      show("Harold could not be reached");
    }
    button.disabled = false;
  });
})();
