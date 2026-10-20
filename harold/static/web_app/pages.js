// The thread page and the todo page, side by side. Swiping is the browser's own scrolling
// of #pages; this script follows which page is in view, keeps the newest message in view,
// and counts the chief's messages that arrive while the todo page is open.
"use strict";

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
    for (const record of records) {
      record.addedNodes.forEach(onNewRow);
    }
  }).observe(messages, { childList: true });

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

  document.addEventListener("input", syncOne);
  document.addEventListener("change", syncOne);
  new MutationObserver(syncSends).observe(pages, { childList: true, subtree: true });

  scrollToBottom();
  syncPage();
  syncSends();
})();
