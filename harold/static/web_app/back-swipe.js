// Back swipe: a swipe right from the left edge opens the page's back link.
(() => {
  const EDGE = 16;
  const DISTANCE = 80;
  const back = document.querySelector("[data-back]");
  if (!back) {
    return;
  }

  let start = null;

  const reset = () => {
    start = null;
  };

  const scrolledSideways = (node) => {
    for (let el = node; el && el.nodeType === 1; el = el.parentElement) {
      if (el.scrollLeft > 0) {
        return true;
      }
    }
    return false;
  };

  const selecting = () => {
    const selection = window.getSelection && window.getSelection();
    return Boolean(selection && selection.toString());
  };

  document.addEventListener(
    "touchstart",
    (event) => {
      if (event.touches.length !== 1) {
        reset();
        return;
      }
      const touch = event.touches[0];
      if (touch.clientX > EDGE || selecting() || scrolledSideways(event.target)) {
        reset();
        return;
      }
      start = { x: touch.clientX, y: touch.clientY, moving: false };
    },
    { passive: true },
  );

  document.addEventListener(
    "touchmove",
    (event) => {
      if (!start) {
        return;
      }
      if (event.touches.length !== 1) {
        reset();
        return;
      }
      const dx = event.touches[0].clientX - start.x;
      const dy = event.touches[0].clientY - start.y;
      if (dx > 0 && Math.abs(dx) > Math.abs(dy)) {
        event.preventDefault();
        start.moving = true;
      }
    },
    { passive: false },
  );

  document.addEventListener(
    "touchend",
    (event) => {
      if (!start) {
        return;
      }
      const touch = event.changedTouches[0];
      const dx = touch.clientX - start.x;
      const dy = touch.clientY - start.y;
      reset();
      if (dx >= DISTANCE && Math.abs(dx) > Math.abs(dy)) {
        location.assign(back.getAttribute("href"));
      }
    },
    { passive: true },
  );

  document.addEventListener("touchcancel", reset, { passive: true });
})();
