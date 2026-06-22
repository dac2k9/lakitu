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

  // 3) Close the inbox drawer via its close button, the backdrop, or Escape.
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

  // 4) Write-actions, taken AS the supervisor — the browser calls /v1 with the
  //    bearer from the (loopback-only) page. The JSON content-type forces a CORS
  //    preflight, so cross-origin can't forge these even setting the token aside.
  function meta(n) {
    var m = document.querySelector('meta[name="' + n + '"]');
    return m ? m.content : "";
  }
  function api(method, path, body) {
    return fetch(path, {
      method: method,
      headers: { Authorization: "Bearer " + meta("lakitu-token"), "Content-Type": "application/json" },
      body: body ? JSON.stringify(body) : undefined,
    });
  }
  function reloadInbox(to) {
    if (window.htmx)
      window.htmx.ajax("GET", "/partial/inbox/" + encodeURIComponent(to), { target: "#drawer", swap: "innerHTML" });
  }
  function reloadBoard() {
    if (window.htmx) window.htmx.ajax("GET", "/partial/board", { target: "#live", swap: "outerHTML" });
  }

  // Send a message (drawer composer): to = the inbox owner, from = you.
  document.addEventListener("submit", function (e) {
    var form = e.target.closest("[data-send-to]");
    if (!form) return;
    e.preventDefault();
    var to = form.getAttribute("data-send-to");
    var title = form.querySelector('[name="title"]').value.trim();
    var body = form.querySelector('[name="body"]').value.trim();
    if (!title || !body) return;
    var btn = form.querySelector('button[type="submit"]');
    if (btn) {
      btn.disabled = true;
      btn.textContent = "sending…";
    }
    api("POST", "/v1/messages", { from: meta("lakitu-me"), to: to, title: title, body: body })
      .then(function (r) {
        if (r.ok) {
          reloadInbox(to);
        } else if (btn) {
          btn.disabled = false;
          btn.textContent = "send";
          alert("send failed (" + r.status + ")");
        }
      })
      .catch(function () {
        if (btn) {
          btn.disabled = false;
          btn.textContent = "send";
        }
      });
  });

  // Check off a task on a card.
  document.addEventListener("click", function (e) {
    var b = e.target.closest("[data-task-done]");
    if (!b) return;
    var owner = b.getAttribute("data-owner");
    var id = b.getAttribute("data-id");
    b.disabled = true;
    b.textContent = "▣";
    api("PATCH", "/v1/agents/" + encodeURIComponent(owner) + "/tasks/" + encodeURIComponent(id), { done: true })
      .then(function (r) {
        if (r.ok) reloadBoard();
        else {
          b.disabled = false;
          b.textContent = "▢";
        }
      })
      .catch(function () {
        b.disabled = false;
        b.textContent = "▢";
      });
  });
})();
