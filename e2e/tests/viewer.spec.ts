import { test, expect, type Page } from "@playwright/test";
import { FLAT_PORT } from "../playwright.config";
import { copyViaButton, identifier, showing, waitForTiles } from "./helpers";

/**
 * The viewer's channel toggles and z/t sliders, driven in a real browser.
 *
 * These assertions are EXACT rather than "something changed", which is the whole reason
 * `tests/fixtures/sample_multidim.ome.zarr` exists. Every (t, c, z) plane in it is a flat value
 *
 *     value(t, c, z) = 80 + 40*t + 15*z
 *
 * and its three channels are pure red, green and blue over a 0-255 window, so the composited
 * output of any control state is computable by hand. A test can therefore sample one pixel of the
 * rendered canvas and know exactly which plane and which channels produced it.
 */

/** Mirrors `plane_value` in `crates/tiling/tests/build_multidim_fixture.rs`. */
function planeValue(t: number, z: number): number {
  return 80 + 40 * t + 15 * z;
}

/**
 * OpenSeadragon requests `default.jpg`, and JPEG is lossy: a flat plane can come back a shade off.
 * Measured worst case on this fixture is 1 (blue-only at z=1 reads (0,1,94) where the same tile as
 * PNG is exactly (0,0,95)), so 3 leaves headroom without letting a real regression through — the
 * differences these tests distinguish are tens to hundreds of levels apart.
 */
const JPEG_TOLERANCE = 3;

function expectPixel(got: number[], want: number[], because: string) {
  const near = got.every((v, i) => Math.abs(v - want[i]) <= JPEG_TOLERANCE);
  expect(
    near,
    `${because}: expected ~[${want}] (±${JPEG_TOLERANCE} for JPEG), got [${got}]`,
  ).toBe(true);
}

/** The centre pixel of the rendered canvas: for this fixture, the composited colour on screen. */
async function centrePixel(page: Page): Promise<number[]> {
  return page.evaluate(() => {
    const c = document.querySelector("#osd canvas") as HTMLCanvasElement;
    const ctx = c.getContext("2d", { willReadFrequently: true })!;
    const d = ctx.getImageData(
      Math.floor(c.width / 2),
      Math.floor(c.height / 2),
      1,
      1,
    ).data;
    return [d[0], d[1], d[2]];
  });
}

/** Sets a range input the way a user would. Callers then assert with `showing(...)`. */
async function setAxis(page: Page, axis: "z" | "t", value: number) {
  await page.locator(`#axis-${axis}`).fill(String(value));
}

async function setChannels(page: Page, on: number[]) {
  // Enable wanted channels first: turning everything off before turning things on would transiently
  // hit the "last channel" lock and leave a box stuck.
  for (const i of on) {
    const box = page.locator(`#ch-${i}`);
    if (!(await box.isChecked())) await box.check();
  }
  for (const i of [0, 1, 2]) {
    if (on.includes(i)) continue;
    const box = page.locator(`#ch-${i}`);
    if ((await box.isChecked()) && (await box.isEnabled())) await box.uncheck();
  }
}

test.beforeEach(async ({ page }) => {
  await page.goto("/viewer/");
  // The controls open on the image's own defaults, so the identifier starts as the literal
  // `default` URL — the same one a viewer with no controls at all would request.
  await showing(page, "default");
});

test("builds its controls from the dimensions endpoint", async ({ page }) => {
  await expect(page.locator("#panel")).toBeVisible();

  // Ranges come from the image's real extents, and open on the default projection's plane.
  await expect(page.locator("#axis-z")).toHaveAttribute("max", "4");
  await expect(page.locator("#axis-z")).toHaveValue("2"); // default z is the middle plane
  await expect(page.locator("#axis-t")).toHaveAttribute("max", "2");
  await expect(page.locator("#axis-t")).toHaveValue("0");

  // Labels and swatches come from omero, not from a guess.
  await expect(page.locator("#channels label")).toHaveText(["Red", "Green", "Blue"]);
  const swatches = await page
    .locator("#channels .swatch")
    .evaluateAll((els) => els.map((e) => getComputedStyle(e).backgroundColor));
  expect(swatches).toEqual(["rgb(255, 0, 0)", "rgb(0, 255, 0)", "rgb(0, 0, 255)"]);

  expectPixel(await centrePixel(page), [110, 110, 110], "default projection is z=2, t=0");
});

test("the z slider selects the plane it names", async ({ page }) => {
  for (const z of [0, 3, 4]) {
    await setAxis(page, "z", z);
    // t stays at its default, so it is not named.
    await showing(page, `@z=${z}`);
    await expect(page.locator('output[for="axis-z"]')).toHaveText(`${z}/4`);
    const v = planeValue(0, z);
    expectPixel(await centrePixel(page), [v, v, v], `z=${z}`);
  }
});

test("the t slider selects the timepoint it names", async ({ page }) => {
  await setAxis(page, "z", 0);
  await showing(page, "@z=0");
  for (const t of [1, 2]) {
    await setAxis(page, "t", t);
    await showing(page, `@z=0,t=${t}`);
    await expect(page.locator('output[for="axis-t"]')).toHaveText(`${t}/2`);
    const v = planeValue(t, 0);
    expectPixel(await centrePixel(page), [v, v, v], `t=${t}`);
  }
});

test("channel toggles composite exactly the selected channels", async ({ page }) => {
  await setAxis(page, "z", 4);
  await setAxis(page, "t", 2);
  await showing(page, "@z=4,t=2");
  const v = planeValue(2, 4); // 220

  const cases: { on: number[]; want: number[]; id: string }[] = [
    { on: [0], want: [v, 0, 0], id: `@z=4,t=2,c=0` },
    { on: [1], want: [0, v, 0], id: `@z=4,t=2,c=1` },
    { on: [2], want: [0, 0, v], id: `@z=4,t=2,c=2` },
    { on: [0, 1], want: [v, v, 0], id: `@z=4,t=2,c=0,c=1` },
    { on: [1, 2], want: [0, v, v], id: `@z=4,t=2,c=1,c=2` },
  ];
  for (const c of cases) {
    await setChannels(page, c.on);
    await showing(page, c.id);
    expectPixel(await centrePixel(page), c.want, `channels [${c.on}] at z=4,t=2`);
  }
});

test("re-enabling every channel drops c= from the identifier", async ({ page }) => {
  // Naming every channel would fragment the tile cache for an image identical to the default.
  await setChannels(page, [0, 1]);
  await showing(page, "@c=0,c=1");
  await setChannels(page, [0, 1, 2]);
  // Back to every default: the identifier collapses all the way to `default`.
  await showing(page, "default");
});

test("the last visible channel cannot be switched off", async ({ page }) => {
  // With no channel selected the server composites nothing, which reads as a broken viewer rather
  // than an empty selection — so the control must not be able to reach that state.
  await setChannels(page, [0]);
  await showing(page, "@c=0");
  await expect(page.locator("#ch-0")).toBeDisabled();
  await expect(page.locator("#ch-0")).toBeChecked();
  await expect(page.locator("#ch-0")).toHaveAttribute(
    "title",
    "At least one channel must stay visible",
  );
  await expect(page.locator("#ch-1")).toBeEnabled();

  // ...and unlocks again as soon as a second channel is on.
  await page.locator("#ch-1").check();
  await expect(page.locator("#ch-0")).toBeEnabled();
});

test("pan and zoom survive a control change", async ({ page }) => {
  const before = await page.evaluate(() => {
    const v = (window as any).OpenSeadragon.getViewer(document.getElementById("osd"));
    v.viewport.zoomTo(3.2, null, true);
    v.viewport.panTo(new (window as any).OpenSeadragon.Point(0.35, 0.42), true);
    const b = v.viewport.getBounds(true);
    return { x: b.x, y: b.y, w: b.width, h: b.height };
  });

  await setAxis(page, "z", 1);
  await showing(page, "@z=1");

  const after = await page.evaluate(() => {
    const v = (window as any).OpenSeadragon.getViewer(document.getElementById("osd"));
    const b = v.viewport.getBounds(true);
    return { x: b.x, y: b.y, w: b.width, h: b.height };
  });

  for (const k of ["x", "y", "w", "h"] as const) {
    expect(
      Math.abs(after[k] - before[k]),
      `viewport ${k} must survive the tile-source reopen`,
    ).toBeLessThan(0.01);
  }
});

test("control changes are real requests, not just a readout", async ({ page }) => {
  const requested: string[] = [];
  page.on("request", (r) => {
    const u = new URL(r.url());
    if (u.pathname.startsWith("/iiif/")) requested.push(decodeURIComponent(u.pathname));
  });

  await setAxis(page, "z", 3);
  await showing(page, "@z=3");
  await setChannels(page, [0]);
  await showing(page, "@z=3,c=0");

  expect(requested.some((p) => p.startsWith("/iiif/@z=3/"))).toBe(true);
  expect(requested.some((p) => p.startsWith("/iiif/@z=3,c=0/"))).toBe(true);
});

// --- Copying the view's URL ------------------------------------------------------------------
//
// The readout used to print the bare identifier (`@z=118,t=0,c=0,c=1`), which is not what a IIIF
// client consumes and cannot be pasted anywhere useful. The copy button next to it now copies the
// URL a client actually wants: this image's own info.json. A test that only checked some text
// appeared would not catch the defect being fixed here, which was exactly that the displayed text
// did not correspond to a real, fetchable resource. So this fetches the copied string and checks
// it is genuinely a IIIF info.json.

test("the copy button copies this view's absolute info.json URL, and it resolves", async ({ page }) => {
  await setAxis(page, "z", 3);
  await showing(page, "@z=3");

  const copied = await copyViaButton(page);
  const origin = new URL(page.url()).origin;
  expect(copied, "must be absolute, not the bare identifier or a relative path").toBe(
    `${origin}/iiif/${encodeURIComponent("@z=3")}/info.json`,
  );

  const info = await page.evaluate(async (url) => {
    const r = await fetch(url, { headers: { Accept: "application/json" } });
    return { status: r.status, body: await r.json() };
  }, copied);
  expect(info.status).toBe(200);
  expect(info.body.type).toBe("ImageService3");
  expect(info.body.protocol).toBe("http://iiif.io/api/image");
});

test("the copy button's URL follows the identifier as controls change", async ({ page }) => {
  await setAxis(page, "z", 4);
  await setChannels(page, [0, 1]);
  await showing(page, "@z=4,c=0,c=1");

  const copied = await copyViaButton(page);
  const origin = new URL(page.url()).origin;
  expect(copied).toBe(`${origin}/iiif/${encodeURIComponent("@z=4,c=0,c=1")}/info.json`);
});

test("the copy button announces success without duplicating the identifier's live region", async ({ page }) => {
  await expect(page.locator("#identifier-live[aria-live]")).toHaveCount(1);
  await expect(page.locator("[aria-live]")).toHaveCount(2); // #identifier-live and #status

  await copyViaButton(page);
  await expect(page.locator("#copy-feedback")).toHaveText("Copied to clipboard");
});

// Stubbing `writeText` (as `copyViaButton` does above) proves the right string reaches the copy
// call, but not that the OS clipboard actually ends up holding it. `127.0.0.1` is a secure
// context, so the real `navigator.clipboard` is available here; granting the permission outright
// means this reads back the genuine system clipboard rather than guessing at browser prompts.
test("the copy button's URL actually reaches the OS clipboard, unstubbed", async ({ page, context }) => {
  const origin = new URL(page.url()).origin;
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin });

  await setAxis(page, "z", 3);
  await showing(page, "@z=3");

  await page.locator("#copy-url").click();
  await expect(page.locator("#copy-feedback")).toHaveText("Copied to clipboard");

  const clipboard = await page.evaluate(() => navigator.clipboard.readText());
  expect(clipboard).toBe(`${origin}/iiif/${encodeURIComponent("@z=3")}/info.json`);
});

test("the copy button is a real, named, keyboard-reachable control", async ({ page }) => {
  const btn = page.getByRole("button", { name: "Copy URL" });
  await expect(btn).toBeEnabled();

  // Stubbed the same way `copyViaButton` stubs it, so pressing Enter (rather than clicking) is
  // provably what triggers the copy, not just that the button exists and can take focus.
  await page.evaluate(() => {
    const w = window as any;
    if (w.navigator.clipboard) {
      w.navigator.clipboard.writeText = (text: string) => {
        w.__zivCopied = text;
        return Promise.resolve();
      };
    }
  });
  await btn.focus();
  await expect(btn).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.locator("#copy-feedback")).toHaveText("Copied to clipboard");
});

test("the panel does not cover the navigation buttons", async ({ page }) => {
  // The panel started life in the top-left, exactly where the zoom/home/full-screen buttons sit,
  // and hid them completely. Hit-testing is what actually proves they are reachable.
  for (const title of ["Zoom in", "Zoom out", "Go home", "Full screen"]) {
    const reachable = await page.evaluate((t) => {
      const el = document.querySelector(`[title="${t}"]`);
      if (!el) return "not-found";
      const b = el.getBoundingClientRect();
      const top = document.elementFromPoint(b.left + b.width / 2, b.top + b.height / 2);
      return el === top || el.contains(top) ? "reachable" : "covered";
    }, title);
    expect(reachable, `${title} must not be covered by the control panel`).toBe("reachable");
  }
});

async function isFullPage(page: Page): Promise<boolean> {
  return page.evaluate(() =>
    (window as any).OpenSeadragon.getViewer(document.getElementById("osd")).isFullPage(),
  );
}

test("the panel stays usable in full page", async ({ page }) => {
  // Full page detaches everything in <body> outside the viewer, which is where the panel used to
  // be, so full page hid every control. Using one proves more than the panel merely showing.
  await page.getByRole("button", { name: "Full screen", exact: true }).click();
  await expect.poll(() => isFullPage(page)).toBe(true);

  await expect(page.locator("#panel")).toBeVisible();
  await setAxis(page, "z", 0);
  await showing(page, "@z=0");
  expectPixel(await centrePixel(page), [80, 80, 80], "z=0 chosen in full page");

  // ...and it is still where it belongs after leaving.
  await page.getByRole("button", { name: "Exit full screen", exact: true }).click();
  await expect.poll(() => isFullPage(page)).toBe(false);
  await expect(page.locator("#panel")).toBeVisible();
});

test("the loading indicator still reports in full page", async ({ page }) => {
  // The same detachment took the indicator too, which on a remote store is the only sign that a
  // slow or failing load is not a broken viewer.
  await page.getByRole("button", { name: "Full screen", exact: true }).click();
  await expect.poll(() => isFullPage(page)).toBe(true);

  await page.evaluate(() => {
    const v = (window as any).OpenSeadragon.getViewer(document.getElementById("osd"));
    v.raiseEvent("tile-load-failed", { tile: { url: "x" } });
  });
  await expect(page.locator("#status")).toBeVisible();
  await expect(page.locator("#status")).toContainText("retrying");
});

test("the panel toggle collapses and expands, and its icon and label follow", async ({ page }) => {
  const toggle = page.locator("#panel-toggle");
  const body = page.locator("#panel-body");

  await expect(body).toBeVisible();
  await expect(toggle).toHaveAccessibleName("Collapse controls");
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await expect(toggle.locator("svg")).toHaveAttribute("data-icon", "chevron-up");

  await toggle.click();
  await expect(body).toBeHidden();
  await expect(toggle).toHaveAccessibleName("Expand controls");
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
  await expect(toggle.locator("svg")).toHaveAttribute("data-icon", "chevron-down");

  // Exactly one icon: the button's contents are replaced on each toggle, not appended to.
  await toggle.click();
  await expect(body).toBeVisible();
  await expect(toggle).toHaveAccessibleName("Collapse controls");
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await expect(toggle.locator("svg")).toHaveCount(1);
  await expect(toggle.locator("svg")).toHaveAttribute("data-icon", "chevron-up");
});

test("the page loads with a clean console", async ({ page }) => {
  // Catches JavaScript exceptions and failed resource loads — a broken OpenSeadragon sprite, a
  // renamed asset, a typo in the control script — none of which change a pixel but all of which
  // are visible to anyone with devtools open.
  //
  // It does NOT guard the inlined favicon, despite that being the bug it was written after:
  // headless Chromium does not request /favicon.ico, so removing the <link> leaves this green.
  // That property is pinned in Rust instead, by `viewer_js_wires_the_controls_to_the_dimensions_endpoint`
  // in crates/server/src/viewer.rs, which asserts the page declares an inline `data:` icon.
  const errors: string[] = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.reload();
  await showing(page, "default");
  expect(errors).toEqual([]);
});

test("an image with a single plane and channel shows no controls at all", async ({ page }) => {
  await page.goto(`http://127.0.0.1:${FLAT_PORT}/viewer/`);
  await waitForTiles(page);

  await expect(page.locator("#panel")).toBeHidden();
  await expect(page.locator("#axes input")).toHaveCount(0);
  await expect(page.locator("#channels input")).toHaveCount(0);

  // ...while the image itself still loads. This fixture is genuinely black, so the proof is
  // OpenSeadragon's own state rather than a pixel colour.
  const state = await page.evaluate(() => {
    const v = (window as any).OpenSeadragon.getViewer(document.getElementById("osd"));
    const item = v.world.getItemAt(0);
    return { open: v.isOpen(), w: item.source.width, h: item.source.height };
  });
  expect(state).toEqual({ open: true, w: 1024, h: 1024 });
});

// --- Loading state --------------------------------------------------------------------------
//
// This viewer waits far longer than a web page usually does: a cold tile from a remote OME-Zarr
// takes 15-25s, because it fetches and decodes a whole compressed chunk per channel. Without a
// signal the viewer looks broken rather than busy. These fixtures are local and therefore instant,
// so what is testable here is the half that must NOT happen — no flicker on a fast load — plus the
// state machine driven directly.

test("a fast load shows no loading indicator at all", async ({ page }) => {
  // The indicator waits 300ms before appearing precisely so an instant load does not flash it.
  const sightings: boolean[] = [];
  for (let i = 0; i < 20; i++) {
    sightings.push(await page.locator("#status").isVisible());
    await page.waitForTimeout(100);
  }
  expect(sightings.some(Boolean), "indicator must not flash on an instant load").toBe(false);
});

test("a control change on a fast image shows no indicator either", async ({ page }) => {
  await setAxis(page, "z", 4);
  await showing(page, "@z=4");
  await expect(page.locator("#status")).toBeHidden();
});

test("a failing tile is reported as retrying, then as failed once its retries run out", async ({ page }) => {
  // Drive the viewer's own handler: a tile failing is what the user must be told about, and it
  // cannot be provoked from a local fixture that always succeeds. `slow-store.spec.ts` covers the
  // real thing, with requests that actually fail.
  //
  // OpenSeadragon reports every failed attempt, retries included, so the same tile is raised each
  // time: a failure the viewer is about to retry must not be announced as a lost tile.
  const raise = () =>
    page.evaluate(() => {
      const w = window as any;
      const v = w.OpenSeadragon.getViewer(document.getElementById("osd"));
      w.__failingTile = w.__failingTile || {};
      v.raiseEvent("tile-load-failed", { tile: w.__failingTile });
      return v.tileRetryMax as number;
    });
  const status = page.locator("#status");

  const retries = await raise();
  expect(retries, "the viewer must retry failed tiles").toBeGreaterThan(0);
  await expect(status).toBeVisible();
  await expect(status).toHaveClass(/warn/);
  await expect(status).toContainText("retrying");
  await expect(status).not.toContainText("failed");

  for (let i = 0; i < retries; i++) await raise();
  await expect(status).toContainText("1 tile failed to load");
  // ...and it says what to do about it, not just that something broke.
  await expect(status).toContainText("lower zoom");
});

test("the indicator is announced to assistive technology", async ({ page }) => {
  // A spinner nobody can perceive is not a loading indicator.
  await expect(page.locator("#status")).toHaveAttribute("role", "status");
  await expect(page.locator("#status")).toHaveAttribute("aria-live", "polite");
  // The spinner itself is decoration; the text carries the meaning.
  await expect(page.locator("#spinner")).toHaveAttribute("aria-hidden", "true");
});

test("the indicator does not cover the controls or the navigation buttons", async ({ page }) => {
  const box = await page.locator("#status").evaluate((el) => {
    (el as HTMLElement).hidden = false;
    const r = el.getBoundingClientRect();
    return { left: r.left, bottom: window.innerHeight - r.bottom, top: r.top };
  });
  // Bottom-left: the panel is top-right and the navigation buttons top-left, so this is the free
  // corner.
  expect(box.left).toBeLessThan(60);
  expect(box.bottom).toBeLessThan(60);
  expect(box.top).toBeGreaterThan(100);
});
