/* sysview site — no dependencies, ES5-friendly */
(function () {
  "use strict";

  /* ---- tabs ---- */
  var tabs = document.querySelectorAll(".tab");
  function selectTab(btn) {
    var target = "panel-" + btn.getAttribute("data-tab");
    tabs.forEach(function (t) {
      t.setAttribute("aria-selected", t === btn ? "true" : "false");
    });
    document.querySelectorAll(".panel").forEach(function (p) {
      p.classList.toggle("hidden", p.id !== target);
    });
  }
  tabs.forEach(function (t) {
    t.addEventListener("click", function () { selectTab(t); });
  });

  /* ---- copy buttons ---- */
  function copyText(text, btn) {
    function done() {
      var old = btn.textContent;
      btn.textContent = "Copied \u2713";
      btn.classList.add("done");
      btn.setAttribute("aria-label", "Copied");
      setTimeout(function () {
        btn.textContent = old;
        btn.classList.remove("done");
        btn.setAttribute("aria-label", "Copy");
      }, 1600);
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { fallback(text, done); });
    } else {
      fallback(text, done);
    }
  }
  function fallback(text, cb) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand("copy"); } catch (e) { /* noop */ }
    document.body.removeChild(ta);
    cb();
  }
  document.querySelectorAll(".copy").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var code = btn.parentNode.querySelector("code");
      copyText(code ? stripPrompts(code.innerText) : "", btn);
    });
  });
  /* copy the raw commands; keep comments but drop the prompt sigils */
  function stripPrompts(t) {
    return t.replace(/^(PS&gt;|>|\$)\s?/gm, "");
  }

  /* ---- mobile nav ---- */
  var toggle = document.getElementById("navToggle");
  var links = document.getElementById("navLinks");
  if (toggle && links) {
    toggle.addEventListener("click", function () {
      var open = links.classList.toggle("open");
      toggle.setAttribute("aria-expanded", open ? "true" : "false");
    });
    links.querySelectorAll("a").forEach(function (a) {
      a.addEventListener("click", function () {
        links.classList.remove("open");
        toggle.setAttribute("aria-expanded", "false");
      });
    });
  }

  /* ---- back-to-top ---- */
  var topLink = document.querySelector(".toplink");
  if (topLink) {
    window.addEventListener("scroll", function () {
      topLink.hidden = window.scrollY < 600;
    }, { passive: true });
  }

  /* ---- year ---- */
  var y = document.getElementById("year");
  if (y) { y.textContent = String(new Date().getFullYear()); }
})();