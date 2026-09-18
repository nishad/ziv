// The built-in viewer, shared by `ziv serve` and every static export. index.html loads it.
(function () {
  "use strict";

  // `static` when this page was written by `ziv export`, `server` when `ziv serve` sent it. See the
  // marker and loader in index.html.
  var STATIC =
    (document.querySelector('meta[name="ziv-mode"]') || {}).content === "static";

  // The viewer is served both at `/viewer/` (the single-image root alias) and at
  // `/i/{name}/viewer/`. Deriving the API base from the page's own location means ONE asset serves
  // both, with no server-side templating and no build step. At the root this resolves to "", which
  // is exactly the pre-multi-image behaviour.
  var BASE = window.location.pathname.replace(/\/viewer\/?$/, "");
  // How many times a failed tile is tried again. The loading report needs it too: see `status`.
  var TILE_RETRIES = 2;
  var IIIF = BASE + "/iiif/";
  var DIMENSIONS = BASE + "/ziv/dimensions.json";
  var state = {
    t: 0, z: 0, defaultT: 0, defaultZ: 0,
    channels: [], sizeT: 1, sizeZ: 1, ready: false,
    // Label images the server offers, and which one (if any) is being drawn. "" means none.
    // `mode` is "overlay" (the mask over the image) or "only" (the mask alone on black).
    labels: [], label: "", palette: "", mode: "overlay", opacity: 0.6,
    // A static export's `ziv/views.json`: which planes and overlays it wrote, and the folder of
    // each. Stays null on a server, which addresses views by identifier instead.
    views: null
  };

  // Constructed with NO tile source. At the root of a multi-image server there is no image to
  // draw, and opening one eagerly put two 404s in the console (info.json and dimensions.json for
  // an image that is not there) before the picker appeared. `start()` below opens one once the
  // page knows there is something to open.
  //
  // OpenSeadragon's own image buttons are replaced by `nav.js`, which draws the same four controls
  // as real buttons. `prefixUrl` still goes through BASE so that anything else of OpenSeadragon's
  // that asks for an image fetches it from this mount rather than relying on the root viewer route.
  //
  // The tile-loading settings are tuned for a store that can take tens of seconds per tile, where
  // OpenSeadragon's defaults lost tiles that were never slow, only queued:
  //
  // - `imageLoaderLimit`: by default OpenSeadragon starts every tile request it wants at once, but
  //   a browser sends at most six to one host and queues the rest. Its per-tile timer starts when
  //   OpenSeadragon asks, not when the browser sends, so queued tiles ran out of time before they
  //   were on the wire. Against a whole-brain image, 24 of 27 "failed" tiles were exactly that.
  // - `timeout`: longer than the server's own 30-second request deadline, so the server's answer
  //   (a tile, or a 408) arrives before the browser gives up. OpenSeadragon's timer does not cancel
  //   the request either, so giving up first would also leave a connection busy for nothing.
  // - `tileRetryMax`: without it a tile that failed once stays missing for good. A second try often
  //   succeeds, because the chunks the first attempt did finish fetching are cached by then.
  var viewer = OpenSeadragon(Object.assign({
    id: "osd",
    prefixUrl: (STATIC ? window.zivAssetBase : BASE + "/viewer/") + "openseadragon/images/",
    showNavigator: false,
    showNavigationControl: false,
    tileRetryMax: TILE_RETRIES
  }, STATIC ? {} : {
    // The slow-store settings above apply to a server. A static export's tiles are pre-rendered
    // files, often served over HTTP/2, where a limit of six would only slow loading down.
    imageLoaderLimit: 6,
    timeout: 45000
  }));
  zivNavigation(viewer);

  // The panel and the loading indicator live inside the viewer's own element, beside OpenSeadragon's
  // container. Full page detaches everything else in <body>, so where the markup puts them they
  // vanished in full page, taking the controls and the only loading signal with them.
  //
  // Not added as OpenSeadragon controls, the way the navigation buttons are: `addControl` writes
  // `display: inline-block` and `position: relative` inline, which would override both the
  // `[hidden]` rules these elements rely on and their own corner positions. Appended after
  // construction so they come after the canvas and the navigation buttons in tab order.
  viewer.element.appendChild(document.getElementById("panel"));
  viewer.element.appendChild(document.getElementById("status"));

  // The IIIF identifier for the current control state.
  //
  // Only components that DIFFER from the image's own defaults are named, so returning to the
  // starting state returns to the literal `default` URL. Two reasons that matters: every distinct
  // identifier is a distinct tile-cache key, so spelling out values the server would have chosen
  // anyway fragments the cache for identical pixels; and it keeps the identifier readout honest —
  // it always names the URL actually being displayed, which is what makes it copyable.
  function identifier() {
    var parts = [];
    if (state.sizeZ > 1 && state.z !== state.defaultZ) parts.push("z=" + state.z);
    if (state.sizeT > 1 && state.t !== state.defaultT) parts.push("t=" + state.t);
    var overlaying = state.label && state.mode === "overlay";
    // An overlay draws ON the intensity render, so the channel selection still applies to it.
    // The mask on its own has no channels to combine, and naming them would add components the
    // server ignores and split the tile cache for identical pixels.
    if (!state.label || overlaying) {
      if (state.channels.length && !allChannelsDefault()) {
        state.channels.forEach(function (c) {
          if (c.on) parts.push("c=" + c.index);
        });
      }
    }
    if (state.label) {
      parts.push((overlaying ? "overlay=" : "label=") + labelSpec(overlaying));
    }
    return parts.length ? "@" + parts.join(",") : "default";
  }

  // `NAME[:PALETTE[:OPACITY]]`. The fields are positional, so naming an opacity means naming a
  // palette too — hence the explicit "distinct" when the default palette is in use but the
  // opacity is not. Anything at its default is left out, to keep the identifier (and the
  // tile-cache key) no longer than the request actually needs.
  function labelSpec(overlaying) {
    var parts = [state.label];
    var namesOpacity = overlaying && state.opacity !== 1;
    if (state.palette || namesOpacity) parts.push(state.palette || "distinct");
    if (namesOpacity) parts.push(String(state.opacity));
    return parts.join(":");
  }

  function allChannelsDefault() {
    return state.channels.every(function (c) { return c.on === c.active; });
  }

  function infoUrl() {
    return IIIF + encodeURIComponent(identifier()) + "/info.json";
  }

  // Reopening a tile source resets the viewport, which would throw away the user's pan and zoom
  // every time they nudge a slider. Capture the bounds first and restore them once the new source
  // is open, so changing plane or channels feels like the image updating in place.
  var reopenQueued = false;
  function reopen() {
    if (!state.ready || reopenQueued) return;
    reopenQueued = true;
    requestAnimationFrame(function () {
      reopenQueued = false;
      var bounds = viewer.world.getItemCount() ? viewer.viewport.getBounds(true) : null;
      viewer.addOnceHandler("open", function () {
        if (bounds) viewer.viewport.fitBounds(bounds, true);
      });
      openCurrent();
      render();
    });
  }

  // The absolute info.json URL for exactly what is on screen: the identifier's own IIIF service on
  // a server, or the folder actually being opened for the current plane and label in a static
  // export. This is the one URL worth showing and copying: a IIIF client (Mirador, and this
  // viewer's own `openFolder`) consumes it to reach every other resource on the image, and an
  // export cannot answer an identifier at all (see `openCurrent`). Returns null only when the
  // export never rendered the current combination, in which case there is nothing to show or copy.
  function currentViewUrl() {
    if (STATIC) {
      var folder = staticFolder();
      return folder === null ? null : folderBase(folder) + "info.json";
    }
    return absoluteUrl(infoUrl());
  }

  function absoluteUrl(url) {
    return new URL(url, window.location.href).href;
  }

  // What to call the current plane/label combination when an export never rendered it. Shared
  // between the readout and `openCurrent`'s own error, so the two describe the same thing the same
  // way.
  function unexportedDescription() {
    return state.label ? state.label + " on z=" + state.z : "z=" + state.z;
  }

  // The readout must never claim an address the thing being viewed cannot serve. A server can
  // always answer its own identifier grammar, so `id <identifier>` stays exactly what it always
  // was: the grammar is real there, and teaching it is part of the readout's value. A static
  // export does not address views by identifier at all (`openCurrent` opens a FOLDER, from
  // `views.json`), so showing the identifier there would display a URL the export cannot serve.
  // Showing the same absolute info.json URL that the copy button copies keeps the two honest and
  // in agreement.
  function render() {
    var url = currentViewUrl();
    var html = STATIC
      ? (url
          ? "<b>url</b> " + escapeHtml(url)
          : "<b>url</b> not exported for " + escapeHtml(unexportedDescription()))
      : "<b>id</b> " + escapeHtml(identifier());
    document.getElementById("identifier").innerHTML = html;
    syncCopyButton(url);
  }

  function syncCopyButton(url) {
    var btn = document.getElementById("copy-url");
    btn.disabled = !url;
    btn.title = url
      ? "Copy the IIIF info.json URL for the current view"
      : "Nothing to copy: this view was not exported";
  }

  // `navigator.clipboard.writeText` needs a secure context (HTTPS, or a browser's own allowance
  // for `localhost`/`127.0.0.1`). A static export is routinely served over plain HTTP from a LAN
  // address or `python3 -m http.server`, where the async API is unavailable or rejects outright.
  // A hidden, selected textarea plus the legacy `execCommand("copy")` is the fallback that still
  // works there. Resolves true/false rather than throwing, so the caller only has to react to
  // whether the copy actually happened.
  function legacyCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    // Off-screen rather than hidden: a hidden or zero-size element cannot be focused or selected
    // in every browser, and both are required for `execCommand("copy")` to have anything to copy.
    ta.style.position = "fixed";
    ta.style.top = "-1000px";
    ta.style.left = "-1000px";
    document.body.appendChild(ta);
    ta.focus();
    ta.select();
    ta.setSelectionRange(0, ta.value.length);
    var ok = false;
    try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
    document.body.removeChild(ta);
    return ok;
  }

  function copyToClipboard(text) {
    if (window.isSecureContext && navigator.clipboard && navigator.clipboard.writeText) {
      return navigator.clipboard.writeText(text).catch(function () {
        return legacyCopy(text) ? Promise.resolve() : Promise.reject(new Error("copy failed"));
      });
    }
    return legacyCopy(text) ? Promise.resolve() : Promise.reject(new Error("copy failed"));
  }

  // Feedback lives in `#copy-feedback`, a sibling of the readout inside the SAME `aria-live`
  // region the readout already uses (`#identifier-live`): one live region announcing both kinds
  // of change, rather than a second one built just for this.
  var copyFeedbackTimer = null;
  function announceCopy(ok) {
    var feedback = document.getElementById("copy-feedback");
    clearTimeout(copyFeedbackTimer);
    feedback.className = ok ? "" : "error";
    feedback.textContent = ok
      ? "Copied to clipboard"
      : "Could not copy. Select and copy the URL above.";
    copyFeedbackTimer = setTimeout(function () {
      feedback.textContent = "";
      feedback.className = "";
    }, 4000);
  }

  document.getElementById("copy-url").addEventListener("click", function () {
    var url = currentViewUrl();
    if (!url) return;
    copyToClipboard(url).then(
      function () { announceCopy(true); },
      function () { announceCopy(false); }
    );
  });

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, function (ch) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch];
    });
  }

  function addAxis(key, label, size, initial) {
    var row = document.createElement("div");
    row.className = "axis";
    var id = "axis-" + key;
    row.innerHTML =
      '<label for="' + id + '">' + label + "</label>" +
      '<input type="range" id="' + id + '" min="0" max="' + (size - 1) +
      '" step="1" value="' + initial + '" data-axis="' + key + '">' +
      '<output for="' + id + '">' + initial + "/" + (size - 1) + "</output>";
    var input = row.querySelector("input");
    var out = row.querySelector("output");
    input.addEventListener("input", function () {
      state[key] = Number(input.value);
      out.textContent = input.value + "/" + (size - 1);
      reopen();
    });
    document.getElementById("axes").appendChild(row);
  }

  function addChannel(ch) {
    var row = document.createElement("div");
    row.className = "channel";
    var id = "ch-" + ch.index;
    var name = ch.label || "Channel " + ch.index;
    row.innerHTML =
      '<input type="checkbox" id="' + id + '" data-channel="' + ch.index + '"' +
      (ch.on ? " checked" : "") + ">" +
      '<span class="swatch" style="background:#' + escapeHtml(ch.color) + '"></span>' +
      '<label for="' + id + '">' + escapeHtml(name) + "</label>";
    row.querySelector("input").addEventListener("change", function (e) {
      ch.on = e.target.checked;
      enforceAtLeastOneChannel();
      reopen();
    });
    document.getElementById("channels").appendChild(row);
  }

  // The server renders nothing at all when no channel is selected, which reads as a broken viewer
  // rather than an empty selection. Keep the last remaining channel checked and disabled, so the
  // control cannot reach that state and says why by being visibly locked.
  //
  // While a label image is showing, every channel box is locked instead: the channels genuinely
  // do not apply, and a live control that changes nothing is worse than a disabled one that says
  // why.
  function enforceAtLeastOneChannel() {
    var showingLabel = !!state.label && state.mode === "only";
    var on = state.channels.filter(function (c) { return c.on; });
    state.channels.forEach(function (c) {
      var box = document.getElementById("ch-" + c.index);
      if (!box) return;
      var lock = showingLabel || (on.length === 1 && c.on);
      box.disabled = lock;
      box.title = showingLabel
        ? "Channels do not apply while a label image is showing"
        : (lock ? "At least one channel must stay visible" : "");
    });
    var note = document.getElementById("channels-note");
    if (note) note.hidden = !showingLabel;
  }

  // The label picker: which label image to show instead of the intensity channels, and which
  // colouring to use.
  //
  // The declared-colours option is offered only by a label that actually declares some. An
  // `image-label` block with an empty colour table renders every value transparent, i.e. a black
  // screen, and an option whose only effect is to blank the viewer is not worth offering.
  function buildLabels(labels) {
    var picker = document.getElementById("label-select");
    labels.forEach(function (l) {
      var opt = document.createElement("option");
      opt.value = l.name;
      opt.textContent = l.name;
      picker.appendChild(opt);
    });
    picker.addEventListener("change", function () {
      state.label = picker.value;
      if (STATIC) {
        var chosen = state.labels.filter(function (l) { return l.name === state.label; })[0];
        if (chosen) state.opacity = chosen.opacity;
      }
      if (!currentLabelHasTable()) state.palette = "";
      syncLabelControls();
      reopen();
    });
    var mode = document.getElementById("mode-select");
    mode.addEventListener("change", function () {
      state.mode = mode.value;
      syncLabelControls();
      reopen();
    });
    var palette = document.getElementById("palette-select");
    palette.addEventListener("change", function () {
      state.palette = palette.value;
      reopen();
    });
    var opacity = document.getElementById("opacity-input");
    var opacityOut = document.getElementById("opacity-output");
    opacity.addEventListener("input", function () {
      state.opacity = Number(opacity.value) / 100;
      opacityOut.textContent = opacity.value + "%";
      reopen();
    });
    document.getElementById("labels-group").hidden = false;
    syncLabelControls();
  }

  function currentLabelHasTable() {
    return state.labels.some(function (l) {
      return l.name === state.label && l.declaredColors > 0;
    });
  }

  function syncLabelControls() {
    var showing = !!state.label;
    document.getElementById("mode-row").hidden = STATIC || !showing;
    // The declared-colour option belongs to a label that declares some: `:table` over an empty
    // colour table renders every value transparent, i.e. nothing at all.
    var showTable = currentLabelHasTable();
    document.getElementById("palette-row").hidden = STATIC || !showTable;
    document.getElementById("palette-select").value = showTable ? state.palette : "";
    document.getElementById("opacity-row").hidden = STATIC || !(showing && state.mode === "overlay");
    enforceAtLeastOneChannel();
  }

  function build(dims) {
    state.sizeT = dims.sizeT;
    state.sizeZ = dims.sizeZ;
    state.defaultT = dims.defaultT;
    state.defaultZ = dims.defaultZ;
    state.t = dims.defaultT;
    state.z = dims.defaultZ;
    state.channels = (dims.channels || []).map(function (c) {
      return { index: c.index, label: c.label, color: c.color, active: c.active, on: c.active };
    });
    // A static export offers the labels it rendered, at the opacity it rendered them; a server
    // offers every label the image declares.
    state.labels = STATIC
      ? state.views.labels.map(function (l) {
          return { name: l.name, declaredColors: 0, opacity: l.opacity };
        })
      : (dims.labels || []).map(function (l) {
          return { name: l.name, declaredColors: l.declaredColors || 0 };
        });
    var exportedPlanes = STATIC ? Object.keys(state.views.planes).length : dims.sizeZ;
    var zSlider = dims.sizeZ > 1 && exportedPlanes > 1;
    var tSlider = !STATIC && dims.sizeT > 1;
    var channelToggles = !STATIC && state.channels.length > 1;

    if (zSlider) addAxis("z", "z", dims.sizeZ, dims.defaultZ);
    if (tSlider) addAxis("t", "t", dims.sizeT, dims.defaultT);
    if (zSlider || tSlider) {
      document.getElementById("axes-group").hidden = false;
    }

    if (channelToggles) {
      state.channels.forEach(addChannel);
      enforceAtLeastOneChannel();
      document.getElementById("channels-group").hidden = false;
    }

    if (state.labels.length) buildLabels(state.labels);

    // Nothing to control: leave the panel hidden rather than show an empty box.
    var hasControls = zSlider || tSlider || channelToggles || state.labels.length > 0;
    if (!hasControls) return;

    document.getElementById("panel").hidden = false;
    state.ready = true;
    render();
  }

  // ---- Loading state -------------------------------------------------------------------
  //
  // This viewer routinely waits far longer than a web page usually does: a cold tile from a
  // remote OME-Zarr can take 15-25 seconds, because it fetches and decodes a whole compressed
  // chunk per channel. Without a signal the viewer looks broken rather than busy, and a tile that
  // times out disappears with no explanation at all.
  var status = {
    el: document.getElementById("status"),
    text: document.getElementById("status-text"),
    // How many attempts have failed, per tile. OpenSeadragon reports every failed attempt, the ones
    // it is about to retry included, so a count of those reports called tiles failed while they
    // were still on their way. A tile has only failed once all 1 + TILE_RETRIES attempts have.
    attempts: new Map(),
    showTimer: null,
    slowTimer: null,
    startedAt: 0
  };

  function statusShow(html, kind) {
    status.el.hidden = false;
    status.el.className = kind || "";
    status.text.innerHTML = html;
  }

  function statusHide() {
    status.el.hidden = true;
    clearTimeout(status.slowTimer);
  }

  function loadingStarted() {
    clearTimeout(status.showTimer);
    clearTimeout(status.slowTimer);
    status.startedAt = Date.now();
    // A local image answers in milliseconds; showing a spinner for that would be a flicker, so
    // the indicator only appears once the wait is long enough to be worth explaining.
    status.showTimer = setTimeout(function () {
      statusShow("<b>Loading tiles…</b>");
    }, 300);
    status.slowTimer = setTimeout(function () {
      statusShow(
        '<b>Still loading…</b> <span id="status-detail">fetching chunks from the ' +
        "remote store; a cold region can take a while</span>"
      );
    }, 6000);
  }

  function loadingFinished() {
    clearTimeout(status.showTimer);
    clearTimeout(status.slowTimer);
    if (!reportFailures()) statusHide();
  }

  // Says what the failures so far amount to: tiles still being retried, or tiles that are gone.
  // Returns false when there is nothing to say.
  function reportFailures() {
    var failed = 0;
    var retrying = 0;
    status.attempts.forEach(function (n) {
      if (n > TILE_RETRIES) failed++;
      else retrying++;
    });
    if (!failed && !retrying) return false;
    clearTimeout(status.showTimer);
    clearTimeout(status.slowTimer);
    if (failed) {
      statusShow(
        "<b>" + failed + (failed === 1 ? " tile" : " tiles") + " failed to load</b> " +
        '<span id="status-detail">the image store did not answer in time, even after ' +
        "retrying. Try a lower zoom, or fewer channels</span>",
        "warn"
      );
    } else {
      statusShow(
        '<b>Some tiles are slow to arrive</b> <span id="status-detail">retrying them; ' +
        "the image store is responding slowly</span>",
        "warn"
      );
    }
    return true;
  }

  // A tile that used up its retries is never asked for again, so once a new round of loading
  // starts it has nothing more to say. Tiles still being retried are kept: their next attempt
  // has to be counted against the ones before it.
  function forgetFailedTiles() {
    status.attempts.forEach(function (n, tile) {
      if (n > TILE_RETRIES) status.attempts.delete(tile);
    });
  }

  // Failures and arrivals from a tile source that has since been replaced (a control change
  // reopens it) belong to an image no longer on screen.
  function isCurrent(e) {
    return !e.tiledImage || e.tiledImage === viewer.world.getItemAt(0);
  }

  // A freshly opened item reports `getFullyLoaded() === true` before it has requested a single
  // tile — nothing is outstanding because nothing has been asked for yet. Treating that as "done"
  // hid the indicator for the whole initial fetch, which on a remote store is the longest and most
  // confusing wait there is. So a load only counts as finished once at least one tile has actually
  // arrived.
  var anyTileLoaded = false;
  viewer.addHandler("tile-loaded", function (e) {
    anyTileLoaded = true;
    if (!isCurrent(e) || !status.attempts.delete(e.tile)) return;
    // A retried tile arrived. It can land after the image already counts as fully loaded, when
    // no other event would update the report, so settle it here.
    var item = viewer.world.getItemAt(0);
    if (item && item.getFullyLoaded()) loadingFinished();
  });

  function watchLoading() {
    var item = viewer.world.getItemCount() ? viewer.world.getItemAt(0) : null;
    if (!item) return;
    item.addHandler("fully-loaded-change", function (e) {
      if (e.fullyLoaded) {
        if (anyTileLoaded) loadingFinished();
      } else {
        // Fires again on pan and zoom, whenever new tiles are needed.
        forgetFailedTiles();
        loadingStarted();
      }
    });
  }

  viewer.addHandler("open", function () {
    status.attempts = new Map();
    watchLoading();
  });
  viewer.addHandler("tile-load-failed", function (e) {
    if (!isCurrent(e)) return;
    status.attempts.set(e.tile, (status.attempts.get(e.tile) || 0) + 1);
    // Report as soon as one fails rather than waiting for the rest to finish: when a whole grid is
    // struggling, "still loading" for another 30 seconds is actively misleading.
    reportFailures();
  });
  viewer.addHandler("open-failed", function (e) {
    statusShow(
      "<b>Could not open the image</b> " +
      '<span id="status-detail">' + escapeHtml((e && e.message) || "the tile source did not load") +
      "</span>",
      "error"
    );
  });
  // A folder's own absolute URL, trailing slash included: `new URL("planes/3/", location)`
  // resolves relative to the PAGE, which is what makes every folder reachable from this one page
  // regardless of which one is currently open. Shared by `openFolder` (which also needs it
  // without the trailing slash, for the tile source's `id`) and `currentViewUrl`.
  function folderBase(folder) {
    return new URL(folder === "." ? "./" : folder + "/", window.location.href).href;
  }

  // A static export's views are folders, each a complete IIIF image service whose info.json says
  // `id: "."`. OpenSeadragon resolves a relative id against the PAGE, not the file, so opening a
  // plane's info.json by URL would fetch its tiles from the export root. Fetching the file and
  // setting the id to the folder's own absolute URL opens every folder correctly from this one page.
  var openSeq = 0;
  function openFolder(folder) {
    var seq = ++openSeq;
    var base = folderBase(folder);
    fetch(base + "info.json", { headers: { Accept: "application/json" } })
      .then(function (r) {
        if (!r.ok) throw new Error(folder + "/info.json: HTTP " + r.status);
        return r.json();
      })
      .then(function (info) {
        if (seq !== openSeq) return; // a newer view was asked for while this one loaded
        info.id = base.replace(/\/$/, "");
        viewer.open(info);
      })
      .catch(function (e) {
        statusShow(
          '<b>Could not open the image</b> <span id="status-detail">' + escapeHtml(e.message) + "</span>",
          "error"
        );
      });
  }

  // Opens the view the controls describe: by identifier from a server, by folder from an export.
  function openCurrent() {
    if (STATIC) {
      var folder = staticFolder();
      if (folder === null) {
        statusShow(
          '<b>Could not open the image</b> <span id="status-detail">this export did not render ' +
          escapeHtml(unexportedDescription()) +
          "</span>",
          "error"
        );
        return;
      }
      openFolder(folder);
    } else {
      viewer.open(infoUrl());
    }
  }

  // The folder views.json gives for the current plane, with or without the current label.
  //
  // Returns null when the export did not render this combination. Today the z slider only appears
  // when every plane was written, so that cannot happen; returning null rather than undefined
  // keeps it a reported error instead of a fetch of "undefined/info.json" if a future export ever
  // writes a subset.
  function staticFolder() {
    var z = String(state.z);
    var planes = state.label
      ? (state.views.labels.filter(function (l) { return l.name === state.label; })[0] || {}).planes
      : state.views.planes;
    var folder = planes && planes[z];
    return folder === undefined ? null : folder;
  }

  // Opens the image this page is for. Start in the loading state immediately: the first thing the
  // viewer does is fetch info.json and then tiles, and on a remote store that is exactly the wait
  // worth reporting.
  function start() {
    loadingStarted();
    if (STATIC) {
      var json = function (url) {
        return fetch(url, { headers: { Accept: "application/json" } })
          .then(function (r) { return r.ok ? r.json() : null; });
      };
      Promise.all([json("ziv/views.json"), json("ziv/dimensions.json")])
        .then(function (both) {
          var views = both[0], dims = both[1];
          // An export from before views.json existed, or one this viewer does not understand, still
          // opens: the root is always the default view.
          if (!views || views.version !== 1 || !dims) {
            openFolder(".");
            return;
          }
          state.views = views;
          build(dims);
          openCurrent();
        })
        .catch(function () { openFolder("."); });
      return;
    }
    viewer.open(infoUrl());
    // A server too old for this endpoint, or one that refuses it because auth is configured and
    // the viewer has no credential, must still show the image — just without controls.
    fetch(DIMENSIONS, { headers: { Accept: "application/json" } })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (dims) { if (dims) build(dims); })
      .catch(function () { /* viewer stays usable without controls */ });
  }

  // Chevrons rather than minus and plus, which the zoom buttons already use for something else.
  // The icon is decoration, so the label says what a click will do and `aria-expanded` says the
  // state it will change.
  var toggle = document.getElementById("panel-toggle");
  function syncToggle(collapsed) {
    var label = collapsed ? "Expand controls" : "Collapse controls";
    toggle.replaceChildren(zivIcon(collapsed ? "chevron-down" : "chevron-up"));
    toggle.title = label;
    toggle.setAttribute("aria-label", label);
    toggle.setAttribute("aria-expanded", String(!collapsed));
  }
  syncToggle(false);
  toggle.addEventListener("click", function () {
    syncToggle(document.getElementById("panel").classList.toggle("collapsed"));
  });

  // At the root of a multi-image server there is no image to open, so offer the catalogue
  // instead. `listable: false` is a real answer rather than an empty list: a lazy directory source
  // cannot enumerate, and saying "no images" would be a lie, so fall back to a name hint there.
  function buildCatalogue(data) {
    var list = document.getElementById("catalogue-list");
    (data.images || []).forEach(function (image) {
      var li = document.createElement("li");
      var a = document.createElement("a");
      a.href = image.href + "viewer/";
      a.textContent = image.name;
      li.appendChild(a);
      list.appendChild(li);
    });
    if (!data.listable) {
      var note = document.getElementById("catalogue-note");
      note.textContent =
        "This server does not list its images. Open one by name: /i/<name>/viewer/";
      note.hidden = false;
    }
    // Nothing to zoom, pan or show full screen, so the navigation buttons have no business being
    // on screen. They live inside the viewer's container, so hiding it takes them with it.
    document.getElementById("osd").hidden = true;
    document.getElementById("catalogue").hidden = false;
  }

  // The one decision this page makes: is there an image here, or a choice to offer?
  //
  // A mounted viewer always has an image, so it opens straight away and pays no extra request. At
  // the root the answer depends on the catalogue, so ask first: a multi-image server has no image
  // at the root and opening one would 404. Anything that goes wrong with that question (an older
  // server with no such endpoint, a 401 because auth is on and the viewer has no credential) falls
  // back to opening, which is what a single-image server has always done.
  if (STATIC || BASE !== "") {
    start();
  } else {
    fetch("/ziv/images.json", { headers: { Accept: "application/json" } })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        if (data && (!data.listable || data.images.length > 1)) {
          buildCatalogue(data);
        } else {
          start();
        }
      })
      .catch(function () { start(); });
  }
})();
