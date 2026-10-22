// The thread page and the todo page, side by side. Swiping is the browser's own scrolling
// of #pages; this script follows which page is in view, keeps the newest message in view,
// counts the chief's messages that arrive while the todo page is open, and keeps your place
// when older messages load above what you are reading.
"use strict";

// A link to the app's own address, in the thread or on a todo card, opens in the app instead
// of Safari. One listener on the document covers the rows and cards the stream draws later.
document.addEventListener("click", (event) => {
  const link = event.target instanceof Element ? event.target.closest("a[href]") : null;
  const own = link !== null && link.origin === location.origin;
  if (!own || event.defaultPrevented) {
    return;
  }
  // A link to a place on this page is left to the page.
  if (link.pathname === location.pathname && link.search === location.search && link.hash) {
    return;
  }
  event.preventDefault();
  location.assign(link.href);
});

// The thread's /updates stream is reopened here, 1 s after one really ended. Datastar cancels
// a @get when a newer one to the same address starts, and the cancelled request still
// announces "finished" in the same task as the newer one's "started"; answering that
// "finished" would cancel the live request in turn and loop. So a "finished" that shares a
// task with a "started" is left alone, and a "started" cancels a reopen still waiting.
// A counter of open requests is not used: Datastar can start the first request before this
// script listens, and the counter would miss that "started".
(function () {
  const REOPEN_AFTER_MS = 1000;
  let pending = null;
  let startedInThisTask = false;
  document.addEventListener("datastar-fetch", (event) => {
    const detail = event.detail;
    if (!detail || !detail.el || detail.el.id !== "messages") {
      return;
    }
    if (detail.type === "started") {
      clearTimeout(pending);
      pending = null;
      startedInThisTask = true;
      setTimeout(() => {
        startedInThisTask = false;
      }, 0);
      return;
    }
    if (detail.type === "finished" && !startedInThisTask) {
      const target = detail.el;
      clearTimeout(pending);
      pending = setTimeout(() => {
        pending = null;
        target.dispatchEvent(new CustomEvent("harold-reopen"));
      }, REOPEN_AFTER_MS);
    }
  });
})();

// A page that leaves the screen says so with a request that outlives it, so a stream the
// phone leaves open does not hold back the chief's pushes. A page shown again opens a new
// stream, which counts it visible once more.
(function () {
  const reportHidden = () => {
    fetch("/page/hidden", {
      method: "POST",
      keepalive: true,
      headers: { "Content-Type": "application/json" },
    }).catch(() => {});
  };
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      reportHidden();
    }
  });
  window.addEventListener("pagehide", reportHidden);
})();

(function () {
  const pages = document.getElementById("pages");
  const thread = document.getElementById("thread");
  const messages = document.getElementById("messages");
  if (!pages || !thread || !messages) {
    return;
  }
  const newMessage = document.getElementById("new-message");
  const indicator = document.getElementById("new-indicator");
  const count = document.getElementById("new-count");
  const reply = document.getElementById("reply");
  // How far from the bottom the thread may be and still follow new messages, in pixels.
  const NEAR_BOTTOM = 80;

  let following = true;
  let unseen = 0;
  // The thread's scroll height as of the last change, to tell how much older rows add.
  let previousHeight = 0;

  function distanceFromBottom() {
    return thread.scrollHeight - thread.scrollTop - thread.clientHeight;
  }

  function scrollToBottom() {
    thread.scrollTop = thread.scrollHeight;
    following = true;
    newMessage.hidden = true;
  }

  function onTodos() {
    return pages.scrollLeft > pages.clientWidth / 2;
  }

  function showCount() {
    count.textContent = unseen > 0 ? String(unseen) : "";
    indicator.hidden = unseen === 0;
  }

  // Sets body.on-todos from the page in view, and empties the count on the thread page.
  function syncPage() {
    const away = onTodos();
    document.body.classList.toggle("on-todos", away);
    if (!away && unseen > 0) {
      unseen = 0;
      showCount();
    }
  }

  function show(page) {
    pages.scrollTo({ left: page.offsetLeft, behavior: "smooth" });
  }

  // Whether a record added message rows ahead of a message row: older rows, loaded above
  // the ones already shown, not new ones at the end.
  function olderRecord(record) {
    let after = record.nextSibling;
    while (after && after.nodeType === Node.TEXT_NODE) {
      after = after.nextSibling;
    }
    if (!(after instanceof Element) || !after.classList.contains("message")) {
      return false;
    }
    return Array.from(record.addedNodes).some(
      (node) => node instanceof Element && node.classList.contains("message"),
    );
  }

  // Older rows push what you read down; Safari does not keep the position by itself, so the
  // scroll position grows by the height they add, before the next paint.
  function keepPlace() {
    const added = thread.scrollHeight - previousHeight;
    if (added > 0) {
      thread.scrollTop += added;
    }
    previousHeight = thread.scrollHeight;
  }

  function onNewRow(row) {
    if (!(row instanceof Element) || !row.classList.contains("message")) {
      return;
    }
    if (onTodos() && row.classList.contains("chief")) {
      unseen += 1;
      showCount();
    }
    if (following) {
      scrollToBottom();
    } else {
      newMessage.hidden = false;
    }
  }

  pages.addEventListener("scroll", syncPage, { passive: true });
  thread.addEventListener(
    "scroll",
    () => {
      following = distanceFromBottom() <= NEAR_BOTTOM;
      if (distanceFromBottom() <= 1) {
        newMessage.hidden = true;
      }
    },
    { passive: true },
  );

  document.getElementById("to-todos").addEventListener("click", (event) => {
    event.preventDefault();
    show(document.getElementById("todos"));
  });
  document.getElementById("to-thread").addEventListener("click", (event) => {
    event.preventDefault();
    show(thread);
  });
  newMessage.addEventListener("click", scrollToBottom);
  if (reply) {
    reply.addEventListener("submit", scrollToBottom);
  }

  // A row added to the thread: followed if the reader was at the bottom, else offered.
  new MutationObserver((records) => {
    let older = false;
    for (const record of records) {
      if (olderRecord(record)) {
        older = true;
        continue;
      }
      record.addedNodes.forEach(onNewRow);
    }
    if (older) {
      keepPlace();
    }
  }).observe(messages, { childList: true });

  // Any other change of height (a new row, the dots, the New message button) moves the
  // baseline older rows are measured from. The dots and the button sit in #thread beside
  // #messages, so they are watched too.
  const heightWatch = new ResizeObserver(() => {
    previousHeight = thread.scrollHeight;
  });
  heightWatch.observe(messages);
  heightWatch.observe(newMessage);
  const dotsBox = document.getElementById("responding");
  if (dotsBox) {
    heightWatch.observe(dotsBox);
  }
  // The script keeps the place itself; the browser's own scroll anchoring would move it twice.
  thread.style.overflowAnchor = "none";

  // The responding dots take room under the newest message.
  const responding = document.getElementById("responding");
  if (responding) {
    new MutationObserver(() => {
      if (following) {
        scrollToBottom();
      }
    }).observe(responding, { childList: true, subtree: true });
  }

  // A question's Send is on only when every decision has a pick and each free pick has
  // words. The stream redraws #todos, so the checks run again after every change to it.
  function syncSend(form) {
    const send = form.querySelector("button.send");
    const decisions = form.querySelectorAll("fieldset");
    if (!send || decisions.length === 0) {
      return;
    }
    // While a note is open (Revise or Reject), Send waits only for the note.
    if (form.getAttribute("data-mode")) {
      const words = form.querySelector("textarea");
      send.disabled = !words || words.value.trim() === "";
      return;
    }
    let ready = true;
    decisions.forEach((decision) => {
      const picked = decision.querySelector("input[type=radio]:checked");
      if (!picked) {
        ready = false;
      } else if (picked.hasAttribute("data-free")) {
        const words = picked.closest("label").querySelector(".free-text");
        if (!words || words.value.trim() === "") {
          ready = false;
        }
      }
    });
    send.disabled = !ready;
  }

  function syncSends() {
    document.querySelectorAll("#todos form.todo-form").forEach(syncSend);
  }

  function syncOne(event) {
    const form = event.target.closest && event.target.closest("form.todo-form");
    if (form) {
      syncSend(form);
    }
  }

  // The card being edited, kept so a redraw of #todos puts it back: its id and question
  // version, the checked options, the text fields, and the focus and caret. A card whose
  // question version changed takes the redrawn state.
  let note = null;
  // How long a note with no focus is kept, in milliseconds.
  const NOTE_WITHOUT_FOCUS_MS = 3000;

  function questionCard(target) {
    if (!(target instanceof Element)) {
      return null;
    }
    const card = target.closest("#todos article.card");
    return card && card.querySelector("form.todo-form fieldset") ? card : null;
  }

  function field(card, name) {
    const input = card.querySelector('input[type=hidden][name="' + name + '"]');
    return input ? input.value : "";
  }

  function capture(card) {
    const radios = {};
    card.querySelectorAll("input[type=radio]:checked").forEach((radio) => {
      radios[radio.name] = radio.value;
    });
    const texts = {};
    card.querySelectorAll("input.free-text, textarea").forEach((text) => {
      texts[text.name] = text.value;
    });
    const active = document.activeElement;
    const focus =
      (active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement) &&
      card.contains(active)
        ? {
            name: active.name,
            value: active.type === "radio" ? active.value : null,
            caret:
              active.type === "text" || active.type === "textarea"
                ? [active.selectionStart, active.selectionEnd]
                : null,
          }
        : null;
    note = {
      at: Date.now(),
      card,
      id: field(card, "id"),
      version: field(card, "version"),
      radios,
      texts,
      focus,
    };
  }

  function restore() {
    // A note with no focus (a tap that took none) is kept only long enough for the redraw
    // of its own save, so picks made in the dash show afterwards.
    if (note && !note.focus && Date.now() - note.at > NOTE_WITHOUT_FOCUS_MS) {
      note = null;
    }
    if (!note) {
      return;
    }
    const card = Array.from(document.querySelectorAll("#todos article.card")).find(
      (candidate) =>
        field(candidate, "id") === note.id && field(candidate, "version") === note.version,
    );
    if (!card) {
      note = null;
      return;
    }
    note.card = card;
    card.querySelectorAll("input[type=radio]").forEach((radio) => {
      const checked = note.radios[radio.name] === radio.value;
      if (radio.checked !== checked) {
        radio.checked = checked;
      }
    });
    let rewritten = false;
    card.querySelectorAll("input.free-text, textarea").forEach((text) => {
      if (text.name in note.texts && text.value !== note.texts[text.name]) {
        text.value = note.texts[text.name];
        rewritten = true;
      }
    });
    const focus = note.focus;
    if (!focus) {
      return;
    }
    const target = Array.from(card.querySelectorAll("input, textarea")).find(
      (input) =>
        input.name === focus.name && (focus.value === null || input.value === focus.value),
    );
    if (!target) {
      return;
    }
    // A morph that rewrote the focused field's value also moved its caret to the end.
    const moved = document.activeElement !== target || rewritten;
    if (document.activeElement !== target) {
      target.focus({ preventScroll: true });
    }
    if (moved && focus.caret && focus.caret[0] !== null) {
      target.setSelectionRange(focus.caret[0], focus.caret[1]);
    }
  }

  function noteEdit(event) {
    const card = questionCard(event.target);
    if (card) {
      capture(card);
    }
  }

  document.addEventListener("input", syncOne);
  document.addEventListener("change", syncOne);
  document.addEventListener("input", noteEdit);
  document.addEventListener("change", noteEdit);
  document.addEventListener("focusin", noteEdit);
  // Leaving the card for good drops the note, so picks made elsewhere show. A card a redraw
  // removed is not connected, so its note stays for the redrawn card.
  document.addEventListener("focusout", () => {
    setTimeout(() => {
      if (note && note.card.isConnected && !note.card.contains(document.activeElement)) {
        note = null;
      }
    }, 0);
  });
  // A redraw that only changes a field's value or checked attribute (the morph of the
  // phone's own save coming back) adds no node, so those are watched too, and so is the
  // card's note mode, which Datastar sets. restore() writes
  // properties only, so it cannot retrigger this.
  new MutationObserver(() => {
    restore();
    syncSends();
  }).observe(pages, {
    childList: true,
    subtree: true,
    attributes: true,
    attributeFilter: ["value", "checked", "data-mode"],
  });

  scrollToBottom();
  previousHeight = thread.scrollHeight;
  syncPage();
  syncSends();
})();
