// Lakitu web cockpit — tiny progressive enhancement (no framework).
//
//  1) Braille spinner for working agents, matching the TUI's 80ms tick. We
//     re-query [data-spin] every frame, so it survives htmx swaps (the board
//     element is replaced every 2s).
//  2) A live local clock in the top bar (the #clock element is outside the
//     swapped region, so it persists).
//
// Read-only: the cockpit mirrors the fleet but performs no writes — no bearer
// token is placed in the page. Write-actions return in a later release behind a
// same-origin CSRF token (kept out of the browser), per protoman's review.
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

  // Close the inbox drawer via its close button, the backdrop, or Escape.
  function closeDrawer() {
    var d = document.getElementById("drawer");
    if (d) d.innerHTML = "";
  }
  document.addEventListener("click", function (e) {
    if (e.target.closest("[data-close-drawer]")) closeDrawer();
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape") closeDrawer();
  });

  // Tabs — toggle the active class (htmx swaps #view via the button's hx-get).
  document.addEventListener("click", function (e) {
    var tab = e.target.closest(".tab");
    if (!tab) return;
    var tabs = tab.parentNode.querySelectorAll(".tab");
    for (var k = 0; k < tabs.length; k++) tabs[k].classList.remove("active");
    tab.classList.add("active");
  });
})();
