import { test, expect } from "@playwright/test";
import { CATALOGUE_PORT } from "../playwright.config";
import { showing, waitForTiles } from "./helpers";

/**
 * The root viewer on a multi-image server.
 *
 * A single-image server keeps serving its image at `/viewer/` through the root alias, which is what
 * every other spec in this directory exercises. This one covers the case that only exists once
 * there is a catalogue: no image to draw, so the page has to offer a choice instead.
 */
const BASE = `http://127.0.0.1:${CATALOGUE_PORT}`;

test("the root viewer lists the catalogue", async ({ page }) => {
  await page.goto(`${BASE}/viewer/`);
  const links = page.locator("#catalogue-list a");
  await expect(links).toHaveCount(2);
  // Sorted, so the listing is stable across restarts rather than reflecting hash order.
  await expect(links.first()).toHaveText("sample_labels");
});

test("choosing an image opens its own viewer", async ({ page }) => {
  await page.goto(`${BASE}/viewer/`);
  await page.locator("#catalogue-list a", { hasText: "sample_multidim" }).click();
  await expect(page).toHaveURL(`${BASE}/i/sample_multidim/viewer/`);
  await waitForTiles(page);
  await showing(page, "default");
});

test("a mounted viewer builds controls for its own image", async ({ page }) => {
  await page.goto(`${BASE}/i/sample_labels/viewer/`);
  await waitForTiles(page);
  // sample_labels carries one label; sample_multidim carries none. Getting this wrong means the
  // viewer fetched the other image's dimensions.
  await expect(page.locator("#labels-group")).toBeVisible();

  await page.goto(`${BASE}/i/sample_multidim/viewer/`);
  await waitForTiles(page);
  await expect(page.locator("#labels-group")).toBeHidden();
});

test("a mounted viewer draws the navigation buttons", async ({ page }) => {
  // The page loads its scripts from the root viewer route while its data comes from the mount, so
  // a mounted page is worth one check of its own that the buttons arrived.
  await page.goto(`${BASE}/i/sample_labels/viewer/`);
  await waitForTiles(page);
  await expect(page.getByRole("button", { name: "Zoom in", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Full screen", exact: true })).toBeVisible();
});

test("a mounted viewer requests tiles from its own mount", async ({ page }) => {
  const tileUrls: string[] = [];
  page.on("request", (r) => {
    if (r.url().includes("/iiif/")) tileUrls.push(r.url());
  });
  await page.goto(`${BASE}/i/sample_labels/viewer/`);
  await waitForTiles(page);
  expect(tileUrls.length).toBeGreaterThan(0);
  for (const url of tileUrls) {
    expect(url, "every tile must come from this image's mount").toContain(
      "/i/sample_labels/iiif/",
    );
  }
});

test("the picker page has nothing to zoom and says nothing to the console", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));

  await page.goto(`${BASE}/viewer/`);
  await expect(page.locator("#catalogue")).toBeVisible();

  // OpenSeadragon is constructed with no tile source here, so the zoom/home/full-screen buttons
  // would be operating on nothing. Earlier versions opened a tile source anyway and put two 404s
  // in the console before the picker appeared.
  await expect(page.locator("#osd")).toBeHidden();
  await expect(page.getByRole("button", { name: "Zoom in", exact: true })).toBeHidden();
  await expect(page.locator("#panel")).toBeHidden();
  expect(errors).toEqual([]);
});
