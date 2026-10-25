// The pair screen: posts the typed code to Harold, and opens the app once it is paired.
"use strict";

(function () {
  const form = document.getElementById("pair-form");
  const code = document.getElementById("pair-code");
  const button = document.getElementById("pair-button");
  if (!form || !code || !button || !document.getElementById("pair-notice")) {
    return;
  }

  // Harold patches #pair-notice for a refusal, so the line is looked up each time.
  function show(text) {
    const notice = document.getElementById("pair-notice");
    notice.textContent = text;
    notice.hidden = !text;
  }

  // Datastar's own module, the one the page loads: its signals and its @post.
  const datastar = import("/static/datastar-1.0.2.js");

  // Sets the `code` signal and posts it through Datastar's @post. Harold answers with the
  // `paired` signal for a code that paired, and patches #pair-notice for a refusal. No retry:
  // a second post would spend the code again.
  async function post() {
    const { actions, getPath, mergePatch } = await datastar;
    mergePatch({ code: code.value });
    await actions.post(
      { el: form, evt: undefined, error: (name) => new Error(name), cleanups: new Map() },
      "/pair",
      {
        filterSignals: { include: /^code$/ },
        openWhenHidden: true,
        retry: "never",
        retryMaxCount: 0,
      },
    );
    return getPath("paired") === true;
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    show("");
    button.disabled = true;
    try {
      if (await post()) {
        location.reload();
        return;
      }
      // A refusal Harold words is patched in; a body it refused (400) or a fault (500)
      // reaches no body in Datastar, so the page words it.
      if (document.getElementById("pair-notice").hidden) {
        show("That code didn't work");
      }
      code.value = "";
    } catch {
      show("Harold could not be reached");
    }
    button.disabled = false;
  });
})();
