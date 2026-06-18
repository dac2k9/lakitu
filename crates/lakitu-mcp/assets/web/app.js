// Lakitu web cockpit — tiny progressive enhancement (no framework).
//
//  1) Braille spinner for working agents, matching the TUI's 80ms tick. We
//     re-query [data-spin] every frame, so it survives htmx swaps (the board
//     element is replaced every 2s).
//  2) A live local clock in the top bar (the #clock element is outside the
//     swapped region, so it persists).
(function () {
  "use strict";
  var FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
  var i = 0;
  var reduce =
    window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  if (!reduce) {
    setInterval(function () {
      i = (i + 1) % FRAMES.length;
      var els = document.querySelectorAll("[data-spin]");
      for (var k = 0; k < els.length; k++) {
        els[k].textContent = FRAMES[i];
      }
    }, 80);
  }

  function tick() {
    var el = document.getElementById("clock");
    if (el) {
      el.textContent = new Date().toLocaleTimeString([], { hour12: false });
    }
  }
  setInterval(tick, 1000);
  tick();
})();
