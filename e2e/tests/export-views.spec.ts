import { test, expect, type BrowserContext, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { LABELS_PORT, REPO_ROOT, ZIV } from "../playwright.config";
import { copyViaButton, imagePixel, serveExport, showing } from "./helpers";

/**
 * The shared viewer in static mode, against real `ziv export --planes/--labels` trees.
 *
 * `sample_multidim` has 5 planes whose every channel reads `80 + 40t + 15z` (default z = 2,
 * default t = 0), so a plane is identified by a pixel. `sample_labels` has one plane and the
 * `nuclei` label, which the live server on LABELS_PORT also serves, so a static overlay can be
 * compared with what `serve` draws for the same identifier. `sample_planes_labels` has 4 planes
 * (default z = 2) AND one label, `cells`, spanning every plane: the only fixture that can prove
 * the label picker's folder lookup follows the z slider rather than staying pinned to whichever
 * plane was showing when the overlay was first selected.
 */

const ORIGIN = "http://ziv-export.test";
const dirs: string[] = [];

function exportFixture(fixture: string, flags: string[]): string {
  const dir = mkdtempSync(join(tmpdir(), "ziv-export-views-"));
  dirs.push(dir);
  execFileSync(ZIV, ["export", resolve(REPO_ROOT, `tests/fixtures/${fixture}`), dir, ...flags]);
  return dir;
}

let planesDir = "";
let labelsDir = "";
let plainDir = "";
let planesLabelsDir = "";
test.beforeAll(() => {
  planesDir = exportFixture("sample_multidim.ome.zarr", ["--planes"]);
  labelsDir = exportFixture("sample_labels.ome.zarr", ["--labels"]);
  plainDir = exportFixture("sample_multidim.ome.zarr", []);
  planesLabelsDir = exportFixture("sample_planes_labels.ome.zarr", ["--planes", "--labels"]);
});
test.afterAll(() => dirs.forEach((d) => rmSync(d, { recursive: true, force: true })));

async function open(page: Page, dir: string) {
  await serveExport(page, ORIGIN, dir);
  await page.goto(`${ORIGIN}/index.html`);
}

/**
 * Waits until OpenSeadragon has `folder` open and fully loaded, and, whenever the panel is
 * showing a readout at all, that the readout names exactly that folder's own absolute info.json
 * URL. The identifier grammar (`@z=0`, `@overlay=nuclei:distinct:0.6`, ...) used to be what was
 * asserted here, but a static export does not address views by identifier: it opens a folder (see
 * `staticFolder` in viewer.js), so the readout now shows that folder's URL instead (spec §B). This
 * keeps the same job the old check did, proving the readout is not merely cosmetic and names the
 * resource actually on screen, against what the readout now honestly says.
 *
 * A plain export (no controls, panel hidden) never renders a readout at all; that case is proven
 * by requiring the identifier element to stay empty rather than by a folder URL.
 */
async function showingFolder(page: Page, folder: string) {
  const base = new URL(folder === "." ? "./" : `${folder}/`, `${ORIGIN}/index.html`).href;
  const sourceId = base.replace(/\/$/, "");
  const infoUrl = `${base}info.json`;
  await expect
    .poll(
      () =>
        page.evaluate(({ sourceId, infoUrl }) => {
          const w = window as any;
          const v = w.OpenSeadragon?.getViewer(document.getElementById("osd"));
          const item = v?.world.getItemAt(0);
          if (!item) return "not-open";
          if (item.source.id !== sourceId) return `source:${item.source.id}`;
          if (!item.getFullyLoaded()) return "loading";
          const panelHidden = (document.getElementById("panel") as HTMLElement | null)?.hidden ?? true;
          const shown = (document.getElementById("identifier")?.textContent || "").trim();
          if (panelHidden) return shown === "" ? "ok" : `unexpected-readout:${shown}`;
          return shown === `url ${infoUrl}` ? "ok" : `readout:${shown}`;
        }, { sourceId, infoUrl }),
      { message: `viewer should be showing ${folder}`, timeout: 20_000 },
    )
    .toBe("ok");
}

async function centrePixel(page: Page): Promise<number[]> {
  return imagePixel(page, 0.5, 0.5);
}

function near(got: number[], want: number[], tolerance = 3) {
  return got.every((v, i) => Math.abs(v - want[i]) <= tolerance);
}

test("the z slider opens the exported plane it names", async ({ page }) => {
  await open(page, planesDir);
  await showingFolder(page, ".");
  expect(near(await centrePixel(page), [110, 110, 110])).toBe(true);

  for (const z of [0, 4]) {
    await page.locator("#axis-z").fill(String(z));
    await showingFolder(page, `planes/${z}`);
    const v = 80 + 15 * z;
    const got = await centrePixel(page);
    expect(near(got, [v, v, v]), `z=${z}: ${got}`).toBe(true);
  }

  await page.locator("#axis-z").fill("2");
  await showingFolder(page, ".");
});

test("controls the export did not render are absent", async ({ page }) => {
  await open(page, planesDir);
  await showingFolder(page, ".");
  await expect(page.locator("#panel")).toBeVisible();
  await expect(page.locator("#axis-t")).toHaveCount(0);
  await expect(page.locator("#channels input")).toHaveCount(0);
  await expect(page.locator("#labels-group")).toBeHidden();
});

test("pan and zoom survive a change of plane", async ({ page }) => {
  await open(page, planesDir);
  await showingFolder(page, ".");
  const bounds = () =>
    page.evaluate(() => {
      const v = (window as any).OpenSeadragon.getViewer(document.getElementById("osd"));
      const b = v.viewport.getBounds(true);
      return [b.x, b.y, b.width, b.height];
    });
  await page.evaluate(() => {
    const w = window as any;
    const v = w.OpenSeadragon.getViewer(document.getElementById("osd"));
    v.viewport.zoomTo(3, null, true);
    v.viewport.panTo(new w.OpenSeadragon.Point(0.3, 0.4), true);
  });
  const before = await bounds();
  await page.locator("#axis-z").fill("3");
  await showingFolder(page, "planes/3");
  const after = await bounds();
  after.forEach((v, i) => expect(Math.abs(v - before[i])).toBeLessThan(0.01));
});

test("the label picker opens the exported overlay, as serve draws it", async ({ page, context }) => {
  await open(page, labelsDir);
  await showingFolder(page, ".");
  const options = await page
    .locator("#label-select option")
    .evaluateAll((els) => els.map((e) => (e as HTMLOptionElement).value));
  expect(options).toEqual(["", "nuclei"]);

  await page.locator("#label-select").selectOption("nuclei");
  await showingFolder(page, "planes/0/labels/0");
  // Mode, colours and opacity were fixed when the export was made.
  await expect(page.locator("#mode-row")).toBeHidden();
  await expect(page.locator("#palette-row")).toBeHidden();
  await expect(page.locator("#opacity-row")).toBeHidden();

  const live = await context.newPage();
  await live.goto(`http://127.0.0.1:${LABELS_PORT}/viewer/`);
  await showing(live, "default");
  await live.locator("#label-select").selectOption("nuclei");
  await showing(live, "@overlay=nuclei:distinct:0.6");

  for (const [fx, fy] of [[0.25, 0.25], [0.75, 0.25], [0.25, 0.75], [0.75, 0.75]]) {
    const exported = await imagePixel(page, fx, fy);
    const served = await imagePixel(live, fx, fy);
    expect(near(exported, served, 6), `(${fx}, ${fy}): export ${exported} vs serve ${served}`).toBe(true);
  }
});

/**
 * `viewer.js`'s `staticFolder()` looks up the label's folder for the CURRENT z, but every fixture
 * that carried a label before `sample_planes_labels` had exactly one z-plane, so that lookup only
 * ever ran at z=0. Moving the z slider while an overlay is selected must open the new plane's
 * overlay tree, not the plane that was showing when the overlay was picked, and not fall back to
 * the plain intensity tree.
 */
test("the z slider moves the open overlay to the new plane, not the old one or intensity", async ({ page }) => {
  await open(page, planesLabelsDir);
  await showingFolder(page, ".");

  await page.locator("#label-select").selectOption("cells");
  await showingFolder(page, "planes/2/labels/0");

  await page.locator("#axis-z").fill("0");
  await showingFolder(page, "planes/0/labels/0");

  await page.locator("#axis-z").fill("3");
  await showingFolder(page, "planes/3/labels/0");

  // Turning the overlay back off at the now-current plane opens that plane's plain intensity
  // tree, not the default plane's.
  await page.locator("#label-select").selectOption("");
  await showingFolder(page, "planes/3");
});

test("a plain export opens with no controls", async ({ page }) => {
  await open(page, plainDir);
  await showingFolder(page, ".");
  await expect(page.locator("#panel")).toBeHidden();
});

test("the page loads with a clean console", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));
  await open(page, planesDir);
  await showingFolder(page, ".");
  await page.locator("#axis-z").fill("1");
  await showingFolder(page, "planes/1");
  expect(errors).toEqual([]);
});

// A plain export (no --planes, no --labels) still writes ziv/views.json and ziv/dimensions.json
// unconditionally (spec §5.1), specifically so nothing 404s in its console. `plainDir` is the same
// `sample_multidim.ome.zarr` fixture as `planesDir`, exported with no flags: a clean-console
// regression that only a `--planes` export happened to avoid would slip past the test above.
test("a plain export loads with a clean console", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));
  await open(page, plainDir);
  await showingFolder(page, ".");
  expect(errors).toEqual([]);
});

/**
 * `viewer.js`'s `start()` promises that "an export from before views.json existed, or one this
 * viewer does not understand, still opens" by falling back to the root whenever `views.json` is
 * missing, unparseable, or carries a `version` this build does not recognise (spec §5.2). Nothing
 * else in this file exercises that branch: every other test's `views.json` is exactly what this
 * build wrote. Both cases below reuse `planesDir` (5 planes) rather than a single-plane export, so
 * a z slider would appear if the fallback did NOT trigger — the assertion distinguishes "fell back
 * correctly" from "there was nothing to show anyway".
 */
test.describe("the views.json fallback", () => {
  test("opens the root with no controls when views.json 404s", async ({ page }) => {
    await serveExport(page, ORIGIN, planesDir);
    await page.route(`${ORIGIN}/ziv/views.json`, (route) =>
      route.fulfill({ status: 404, body: "not found" }),
    );
    await page.goto(`${ORIGIN}/index.html`);
    await showingFolder(page, ".");
    await expect(page.locator("#panel")).toBeHidden();
    await expect(page.locator("#axis-z")).toHaveCount(0);
  });

  test("opens the root with no controls when views.json's version is not 1", async ({ page }) => {
    await serveExport(page, ORIGIN, planesDir);
    await page.route(`${ORIGIN}/ziv/views.json`, (route) =>
      route.fulfill({
        contentType: "application/json",
        body: JSON.stringify({
          version: 2,
          defaultZ: 2,
          planes: { "0": "planes/0", "1": "planes/1", "2": ".", "3": "planes/3", "4": "planes/4" },
          labels: [],
        }),
      }),
    );
    await page.goto(`${ORIGIN}/index.html`);
    await showingFolder(page, ".");
    await expect(page.locator("#panel")).toBeHidden();
    await expect(page.locator("#axis-z")).toHaveCount(0);
  });
});

// --- Copying the view's URL ------------------------------------------------------------------
//
// An export does not address views by identifier at all: `openCurrent` opens a FOLDER, chosen
// from `ziv/views.json`. The readout and copy button both name that folder's own absolute
// info.json URL rather than an identifier the export cannot serve. A test that only checked some
// text appeared would not catch that defect, so these fetch the copied string, in a brand new
// page over the same served export, and check it is genuinely fetchable and parses as a IIIF
// info.json.

/** Opens a second page onto the same served export and confirms `url` is absolute and resolves
 *  there as a IIIF info.json, independent of whatever document `url` was copied from. */
async function assertResolvesAsInfoJson(context: BrowserContext, dir: string, url: string) {
  expect(url, "must be absolute, not a bare folder path").toMatch(/^http:\/\//);
  const fresh = await context.newPage();
  await serveExport(fresh, ORIGIN, dir);
  await fresh.goto(`${ORIGIN}/index.html`);
  const info = await fresh.evaluate(async (u) => {
    const r = await fetch(u, { headers: { Accept: "application/json" } });
    return { status: r.status, body: await r.json() };
  }, url);
  await fresh.close();
  expect(info.status).toBe(200);
  expect(info.body.type).toBe("ImageService3");
  expect(info.body.protocol).toBe("http://iiif.io/api/image");
}

test("the copy button copies the open folder's absolute info.json URL, and it resolves", async ({ page, context }) => {
  await open(page, planesDir);
  await showingFolder(page, ".");

  let copied = await copyViaButton(page);
  expect(copied).toBe(`${ORIGIN}/info.json`);
  await assertResolvesAsInfoJson(context, planesDir, copied);

  await page.locator("#axis-z").fill("3");
  await showingFolder(page, "planes/3");
  copied = await copyViaButton(page);
  expect(copied).toBe(`${ORIGIN}/planes/3/info.json`);
  await assertResolvesAsInfoJson(context, planesDir, copied);
});

test("the copy button follows the label picker to the overlay folder actually served", async ({ page, context }) => {
  await open(page, labelsDir);
  await showingFolder(page, ".");

  await page.locator("#label-select").selectOption("nuclei");
  await showingFolder(page, "planes/0/labels/0");

  const copied = await copyViaButton(page);
  expect(copied).toBe(`${ORIGIN}/planes/0/labels/0/info.json`);
  await assertResolvesAsInfoJson(context, labelsDir, copied);
});

test("a view the export never rendered has nothing to copy, and says so honestly", async ({ page }) => {
  // Fabricates a `views.json` naming a label whose folder map has a gap at z=4, the situation
  // `staticFolder()`'s own doc comment says today's exporter cannot produce (every plane gets
  // every label) but a future or hand-edited export might. The readout must not claim a folder
  // this export cannot serve, and the copy button must not offer to copy one.
  await serveExport(page, ORIGIN, planesDir);
  await page.route(`${ORIGIN}/ziv/views.json`, (route) =>
    route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({
        version: 1,
        defaultZ: 2,
        planes: { "0": "planes/0", "1": "planes/1", "2": ".", "3": "planes/3", "4": "planes/4" },
        labels: [
          {
            index: 0,
            name: "fake",
            palette: "distinct",
            opacity: 0.6,
            // No "4": the gap `staticFolder()` is written to detect.
            planes: { "0": "planes/0/labels/0", "1": "planes/1/labels/0", "2": "planes/2/labels/0", "3": "planes/3/labels/0" },
          },
        ],
      }),
    }),
  );
  await page.goto(`${ORIGIN}/index.html`);
  await showingFolder(page, ".");

  await page.locator("#label-select").selectOption("fake");
  await page.locator("#axis-z").fill("4");

  await expect(page.locator("#copy-url")).toBeDisabled();
  await expect(page.locator("#identifier")).toHaveText("url not exported for fake on z=4");
});

// `ORIGIN` (`http://ziv-export.test`) is not a secure context, so `navigator.clipboard` is not
// even defined there: Chromium requires a secure context for the whole interface, which is
// exactly why the viewer falls back to `execCommand` there, proven by the stubbed tests above.
// Proving the OTHER path, that the real OS clipboard actually ends up holding the copied text,
// needs an origin the browser treats as secure. `127.0.0.1` qualifies without TLS, and
// `serveExport`'s `page.route` interception works identically there: nothing has to be listening
// on this port, the route always answers, and this is still a REAL export directory `ziv export`
// wrote to disk, exactly as it would be if hosted over HTTPS (GitHub Pages, S3, a CDN).
const SECURE_ORIGIN = "http://127.0.0.1:59713";

test("the copy button's URL actually reaches the OS clipboard, unstubbed (exported directory)", async ({
  page,
  context,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: SECURE_ORIGIN });
  await serveExport(page, SECURE_ORIGIN, planesDir);
  await page.goto(`${SECURE_ORIGIN}/index.html`);

  const base = new URL("planes/3/", `${SECURE_ORIGIN}/index.html`).href;
  await page.locator("#axis-z").fill("3");
  await expect(page.locator("#identifier")).toHaveText(`url ${base}info.json`);

  await page.locator("#copy-url").click();
  await expect(page.locator("#copy-feedback")).toHaveText("Copied to clipboard");

  const clipboard = await page.evaluate(() => navigator.clipboard.readText());
  expect(clipboard).toBe(`${base}info.json`);
});
