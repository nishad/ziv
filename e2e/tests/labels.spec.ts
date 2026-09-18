import { test, expect } from "@playwright/test";
import { LABELS_PORT } from "../playwright.config";
import { imagePixel, showing, waitForTiles } from "./helpers";

/**
 * The viewer's label controls, driven in a real browser.
 *
 * Fixture: `tests/fixtures/sample_labels.ome.zarr`. The image is a red horizontal ramp plus a blue
 * vertical one; its `nuclei` label is four flat quadrants valued 0/1/2/3, whose declared colours
 * are transparent, red, green and half-alpha blue. So each control state has an exactly computable
 * colour at each quadrant centre, and "the viewer is showing the label" is a claim about pixels
 * rather than about a dropdown having changed value.
 */

/** OpenSeadragon requests `default.jpg`; flat blocks survive it well away from their edges. */
const JPEG_TOLERANCE = 6;

function expectPixel(got: number[], want: number[], because: string) {
  const near = got.every((v, i) => Math.abs(v - want[i]) <= JPEG_TOLERANCE);
  expect(
    near,
    `${because}: expected ~[${want}] (±${JPEG_TOLERANCE} for JPEG), got [${got}]`,
  ).toBe(true);
}

/** The four quadrant centres of the fixture, in image fractions. */
const QUADRANTS: Array<[number, number]> = [
  [0.25, 0.25],
  [0.75, 0.25],
  [0.25, 0.75],
  [0.75, 0.75],
];

async function quadrants(page: import("@playwright/test").Page) {
  const out: number[][] = [];
  for (const [fx, fy] of QUADRANTS) out.push(await imagePixel(page, fx, fy));
  return out;
}

test.beforeEach(async ({ page }) => {
  await page.goto(`http://127.0.0.1:${LABELS_PORT}/viewer/`);
  await showing(page, "default");
});

test("offers the label images the server declares", async ({ page }) => {
  await expect(page.locator("#labels-group")).toBeVisible();
  const options = await page
    .locator("#label-select option")
    .evaluateAll((els) => els.map((e) => (e as HTMLOptionElement).value));
  // "" is the image itself; the fixture declares exactly one label.
  expect(options).toEqual(["", "nuclei"]);

  // The mode, palette and opacity all belong to a label, so none is offered until one is chosen.
  await expect(page.locator("#mode-row")).toBeHidden();
  await expect(page.locator("#palette-row")).toBeHidden();
  await expect(page.locator("#opacity-row")).toBeHidden();
});

test("selecting a label overlays it on the image", async ({ page }) => {
  // Overlay is the default mode: a segmentation is only interesting next to the pixels it
  // segments, and the picker opens on the view people actually want.
  const before = await quadrants(page);
  await page.locator("#label-select").selectOption("nuclei");
  await showing(page, "@overlay=nuclei:distinct:0.6");

  await expect(page.locator("#mode-select")).toHaveValue("overlay");
  await expect(page.locator("#opacity-row")).toBeVisible();

  // Top-left is label 0 — no object, so the image shows through unchanged. The other quadrants
  // are masked, so they must differ from the plain image underneath.
  const after = await quadrants(page);
  expectPixel(after[0], before[0], "background quadrant is untouched");
  for (const i of [1, 2, 3]) {
    expect(
      after[i].some((v, k) => Math.abs(v - before[i][k]) > 20),
      `quadrant ${i} should be visibly overlaid: ${before[i]} -> ${after[i]}`,
    ).toBe(true);
  }
});

test("channel toggles stay live while overlaying, and lock when the label is alone", async ({
  page,
}) => {
  await page.locator("#label-select").selectOption("nuclei");
  await showing(page, "@overlay=nuclei:distinct:0.6");

  // The base of an overlay IS the intensity render, so the channels still matter — and changing
  // one has to reach the server, not just the checkbox.
  await expect(page.locator("#channels-note")).toBeHidden();
  await expect(page.locator("#ch-0")).toBeEnabled();
  await page.locator("#ch-1").uncheck();
  await showing(page, "@c=0,overlay=nuclei:distinct:0.6");
  await page.locator("#ch-1").check();
  await showing(page, "@overlay=nuclei:distinct:0.6");

  // A mask on its own has nothing to combine, so every box locks and says why.
  await page.locator("#mode-select").selectOption("only");
  await showing(page, "@label=nuclei");
  await expect(page.locator("#channels-note")).toBeVisible();
  await expect(page.locator("#ch-0")).toBeDisabled();
  await expect(page.locator("#ch-1")).toBeDisabled();
  await expect(page.locator("#opacity-row")).toBeHidden();

  // ...and unlock again on the way back.
  await page.locator("#mode-select").selectOption("overlay");
  await showing(page, "@overlay=nuclei:distinct:0.6");
  await expect(page.locator("#ch-0")).toBeEnabled();
});

// Dropping a channel must change what is UNDER the mask and nothing else.
test("the overlay draws over the selected channels", async ({ page }) => {
  await page.locator("#label-select").selectOption("nuclei");
  await page.locator("#palette-select").selectOption("table");
  await page.locator("#opacity-input").fill("100");
  await showing(page, "@overlay=nuclei:table");
  const both = await quadrants(page);

  await page.locator("#ch-1").uncheck();
  await showing(page, "@c=0,overlay=nuclei:table");
  const across = await quadrants(page);

  // Top-left has no mask: the base shows through, and it lost its blue.
  expect(both[0][2]).toBeGreaterThan(across[0][2] + 20);
  // Top-right is an opaque declared red: identical either way.
  expectPixel(across[1], both[1], "an opaque mask hides whatever is under it");
  expectPixel(across[1], [255, 0, 0], "label 1 is declared red");
});

test("opacity is dropped from the identifier when it is fully opaque", async ({ page }) => {
  await page.locator("#label-select").selectOption("nuclei");
  await showing(page, "@overlay=nuclei:distinct:0.6");
  await page.locator("#opacity-input").fill("100");
  // Nothing left that differs from the server's own defaults, so nothing is named. Same rule as
  // the channel toggles: a shorter identifier is a smaller tile-cache key for identical pixels.
  await showing(page, "@overlay=nuclei");
});

test("a zero-opacity overlay shows the plain image", async ({ page }) => {
  const plain = await quadrants(page);
  await page.locator("#label-select").selectOption("nuclei");
  await showing(page, "@overlay=nuclei:distinct:0.6");
  await page.locator("#opacity-input").fill("0");
  await showing(page, "@overlay=nuclei:distinct:0");
  const faded = await quadrants(page);
  for (const [i, c] of faded.entries()) {
    expectPixel(c, plain[i], `quadrant ${i} at zero opacity`);
  }
});

test("the label can be shown on its own", async ({ page }) => {
  await page.locator("#label-select").selectOption("nuclei");
  await page.locator("#mode-select").selectOption("only");
  await showing(page, "@label=nuclei");

  // The default palette gives every value its own colour. Background stays black; the other three
  // quadrants must be three different, non-black colours.
  const [bg, ...objects] = await quadrants(page);
  expectPixel(bg, [0, 0, 0], "label 0 is background");
  for (const [i, c] of objects.entries()) {
    expect(c.some((v) => v > 40), `object ${i + 1} should be visible, got [${c}]`).toBe(true);
  }
  const keys = objects.map((c) => c.map((v) => Math.round(v / 32)).join(","));
  expect(new Set(keys).size, `three objects should be three colours: ${keys}`).toBe(3);
});

test("the declared-colour palette renders exactly what the image declares", async ({ page }) => {
  await page.locator("#label-select").selectOption("nuclei");
  await page.locator("#mode-select").selectOption("only");
  await showing(page, "@label=nuclei");
  await page.locator("#palette-select").selectOption("table");
  await showing(page, "@label=nuclei:table");

  const [bg, one, two, three] = await quadrants(page);
  expectPixel(bg, [0, 0, 0], "label 0 has no entry");
  expectPixel(one, [255, 0, 0], "label 1 is declared red");
  expectPixel(two, [0, 255, 0], "label 2 is declared green");
  // Declared as (0, 0, 255, 128): half-alpha blue over black.
  expectPixel(three, [0, 0, 128], "label 3 is declared blue at half alpha");
});

test("returning to Image restores the intensity render", async ({ page }) => {
  await page.locator("#label-select").selectOption("nuclei");
  await showing(page, "@overlay=nuclei:distinct:0.6");
  await page.locator("#label-select").selectOption("");
  // Back to the literal `default` identifier, not a spelled-out equivalent — the same URL a
  // viewer with no controls at all would request.
  await showing(page, "default");

  // The image is a horizontal ramp, which is nothing like the label quadrants: the right half is
  // bright and the left dark, top and bottom alike.
  const [tl, tr, bl, br] = await quadrants(page);
  expect(tr[0]).toBeGreaterThan(tl[0] + 60);
  expect(br[0]).toBeGreaterThan(bl[0] + 60);
});

test("an image with no labels offers no label control", async ({ page }) => {
  // The multidim fixture (this project's default baseURL) has no `labels/` group.
  await page.goto("/viewer/");
  await waitForTiles(page);
  await expect(page.locator("#panel")).toBeVisible();
  await expect(page.locator("#labels-group")).toBeHidden();
  await expect(page.locator("#label-select option")).toHaveCount(1);
});
