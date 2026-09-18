/*
 * ziv's navigation buttons for OpenSeadragon: zoom in, zoom out, home and full screen.
 *
 * They replace OpenSeadragon's own image-sprite buttons, so a viewer using this passes
 * `showNavigationControl: false` and then calls `zivNavigation(viewer)`. The live viewer and every
 * static export load this same file, so the two cannot drift apart.
 *
 * Each handler does exactly what the OpenSeadragon 5.0.1 button it replaces does; only the drawing
 * changed. The buttons are real <button> elements, so they take keyboard focus and carry an
 * accessible name, which the sprite buttons did not.
 *
 * It also holds the only copy of the viewer's icons: `zivIcon(name)` returns one as an SVG element,
 * which the live viewer uses for its panel toggle.
 *
 * Icons are Lucide 1.45.0 (ISC), and every one here except house derives from Feather (MIT). Both
 * notices are in LICENSE-lucide.txt beside this file. The path data is inlined rather than fetched
 * so the viewer keeps working offline and straight off file://.
 */
(function () {
  "use strict";

  var SVG_NS = "http://www.w3.org/2000/svg";

  // Copied unchanged from lucide-static 1.45.0. Every icon is plain paths on a 24x24 grid.
  var ICONS = {
    plus: ["M5 12h14", "M12 5v14"],
    minus: ["M5 12h14"],
    house: [
      "M15 21v-8a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v8",
      "M3 10a2 2 0 0 1 .709-1.528l7-6a2 2 0 0 1 2.582 0l7 6A2 2 0 0 1 21 10v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"
    ],
    maximize: [
      "M8 3H5a2 2 0 0 0-2 2v3",
      "M21 8V5a2 2 0 0 0-2-2h-3",
      "M3 16v3a2 2 0 0 0 2 2h3",
      "M16 21h3a2 2 0 0 0 2-2v-3"
    ],
    minimize: [
      "M8 3v3a2 2 0 0 1-2 2H3",
      "M21 8h-3a2 2 0 0 1-2-2V3",
      "M3 16h3a2 2 0 0 1 2 2v3",
      "M16 21v-3a2 2 0 0 1 2-2h3"
    ],
    "chevron-up": ["m18 15-6-6-6 6"],
    "chevron-down": ["m6 9 6 6 6-6"]
  };

  // Matched to the viewer's control panel: the same translucent surface, border and focus blue, so
  // the buttons and the panel read as one set. Injected into <head> rather than written into each
  // page, and into <head> specifically because full-page mode detaches everything in <body>.
  var CSS = [
    ".ziv-nav { margin: 12px; }",
    ".ziv-nav-stack { display: flex; flex-direction: column; gap: 8px; }",
    ".ziv-nav-group {",
    "  display: flex; flex-direction: column; overflow: hidden;",
    "  background: rgba(24, 24, 26, 0.92);",
    "  border: 1px solid #3a3a3f; border-radius: 8px;",
    "  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.4);",
    "}",
    ".ziv-nav button {",
    "  display: grid; place-items: center; width: 32px; height: 32px;",
    "  margin: 0; padding: 0; border: 0; background: none;",
    "  color: #e6e6e8; cursor: pointer;",
    "}",
    ".ziv-nav button + button { border-top: 1px solid #3a3a3f; }",
    ".ziv-nav button:hover { background: #2e2e34; }",
    ".ziv-nav button:active { background: #232328; }",
    // Inset, because the group clips its children to the rounded corners.
    ".ziv-nav button:focus-visible { outline: 2px solid #6aa9ff; outline-offset: -2px; }",
    ".ziv-nav svg { width: 18px; height: 18px; }",
    // A 32px target is fine for a mouse and too small for a finger.
    "@media (pointer: coarse) {",
    "  .ziv-nav button { width: 40px; height: 40px; }",
    "  .ziv-nav svg { width: 20px; height: 20px; }",
    "}"
  ].join("\n");

  function injectStyle() {
    if (document.getElementById("ziv-nav-style")) return;
    var style = document.createElement("style");
    style.id = "ziv-nav-style";
    style.textContent = CSS;
    document.head.appendChild(style);
  }

  // Lucide's own stroke attributes travel with every icon, so one renders correctly wherever it is
  // placed and a stylesheet only has to size it.
  function icon(name) {
    var svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    svg.setAttribute("data-icon", name);
    ICONS[name].forEach(function (d) {
      var path = document.createElementNS(SVG_NS, "path");
      path.setAttribute("d", d);
      svg.appendChild(path);
    });
    return svg;
  }

  // `title` gives the hover tooltip and `aria-label` the accessible name. They are kept identical
  // so what a sighted mouse user reads is what a screen reader announces.
  function setLabel(el, text) {
    el.title = text;
    el.setAttribute("aria-label", text);
  }

  function button(label, iconName, onClick) {
    var b = document.createElement("button");
    b.type = "button";
    setLabel(b, label);
    b.appendChild(icon(iconName));
    b.addEventListener("click", onClick);
    return b;
  }

  function group(buttons) {
    var g = document.createElement("div");
    g.className = "ziv-nav-group";
    buttons.forEach(function (b) { g.appendChild(b); });
    return g;
  }

  function zivNavigation(viewer) {
    injectStyle();

    var zoomIn = button("Zoom in", "plus", function () {
      if (!viewer.viewport) return;
      viewer.viewport.zoomBy(viewer.zoomPerClick);
      viewer.viewport.applyConstraints();
    });
    var zoomOut = button("Zoom out", "minus", function () {
      if (!viewer.viewport) return;
      viewer.viewport.zoomBy(1 / viewer.zoomPerClick);
      viewer.viewport.applyConstraints();
    });
    var home = button("Go home", "house", function () {
      if (viewer.viewport) viewer.viewport.goHome();
    });
    var fullScreen = button("Full screen", "maximize", function () {
      // Mirrors OpenSeadragon's own button. A full page entered without the Fullscreen API has no
      // full screen to exit, and `setFullScreen(false)` would do nothing, so leave it directly.
      if (viewer.isFullPage() && !OpenSeadragon.isFullScreen()) {
        viewer.setFullPage(false);
      } else {
        viewer.setFullScreen(!viewer.isFullPage());
      }
      // Entering and leaving full page moves the viewer around the DOM, which drops focus.
      fullScreen.focus();
    });
    fullScreen.setAttribute("aria-pressed", "false");

    // Driven by OpenSeadragon's events rather than by the click, so the icon is also right after
    // the user leaves full screen with Escape.
    function syncFullScreen() {
      var on = viewer.isFullPage();
      fullScreen.replaceChild(icon(on ? "minimize" : "maximize"), fullScreen.firstChild);
      setLabel(fullScreen, on ? "Exit full screen" : "Full screen");
      fullScreen.setAttribute("aria-pressed", String(on));
    }
    viewer.addHandler("full-page", syncFullScreen);
    viewer.addHandler("full-screen", syncFullScreen);

    // OpenSeadragon sets `display: inline-block` on whatever element it is given, so the flex
    // layout lives on an inner element it does not touch.
    var nav = document.createElement("div");
    nav.className = "ziv-nav";
    nav.setAttribute("role", "group");
    nav.setAttribute("aria-label", "Image navigation");
    var stack = document.createElement("div");
    stack.className = "ziv-nav-stack";
    stack.appendChild(group([zoomIn, zoomOut]));
    stack.appendChild(group([home, fullScreen]));
    nav.appendChild(stack);

    // A control rather than a sibling of the viewer, so it survives full page, which detaches
    // everything outside the viewer. `autoFade: false` because the rest of the viewer's chrome
    // never fades, and buttons that vanish two seconds after the mouse stops are hard to find.
    viewer.addControl(nav, { anchor: OpenSeadragon.ControlAnchor.TOP_LEFT, autoFade: false });
    return nav;
  }

  window.zivNavigation = zivNavigation;
  window.zivIcon = icon;
})();
