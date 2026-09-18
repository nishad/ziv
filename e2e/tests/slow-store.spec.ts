import { test, expect, type Route } from "@playwright/test";
import { FLAT_PORT } from "../playwright.config";
import { waitForTiles } from "./helpers";

/**
 * How the viewer fetches tiles when the image store is slow.
 *
 * A remote OME-Zarr can take tens of seconds per tile. Against IDR's whole-brain image
 * (idr0048A 9846152, 19120x13350) the viewer reported 27 tiles failed on one plane, and 24 of those
 * were OpenSeadragon's own 30-second timer rather than anything the server did: it had started 29
 * tile requests at once, the browser sends six to a host, and the queued ones ran out of time
 * before they were ever sent. It never retried them, so they stayed missing.
 *
 * Nothing here needs a slow server. `page.route` makes the browser's side of a slow store
 * directly: requests that fail once, and requests that take a while to answer.
 */

const BASE = `http://127.0.0.1:${FLAT_PORT}`;

function isTile(url: URL) {
  return url.pathname.startsWith("/iiif/") && url.pathname.endsWith("/default.jpg");
}

test("a tile that fails is retried, and reported as slow rather than failed", async ({ page }) => {
  // Every tile fails its first attempt, the way one does when the store is too slow once.
  const attempts = new Map<string, number>();
  await page.route(isTile, async (route: Route) => {
    const url = route.request().url();
    const n = (attempts.get(url) ?? 0) + 1;
    attempts.set(url, n);
    if (n === 1) return route.abort("timedout");
    return route.continue();
  });

  await page.goto(`${BASE}/viewer/`);
  // While the retries are pending the report says they are coming, not that they are lost.
  await expect(page.locator("#status")).toContainText("retrying");

  await expect
    .poll(() => attempts.size > 0 && [...attempts.values()].every((n) => n >= 2), {
      message: "every tile that failed must be asked for again",
      timeout: 30_000,
    })
    .toBe(true);
  await waitForTiles(page);
  // ...and once they have all arrived there is nothing left to report.
  await expect(page.locator("#status")).toBeHidden();
});

test("no more tile requests are started than the browser will send at once", async ({ page }) => {
  // A tall viewport over a large image, so the first view wants far more tiles than six.
  await page.setViewportSize({ width: 2560, height: 1440 });

  // Describe the flat fixture as an 8192x8192 image. Every tile below is served from memory, so
  // the size is free, and the only thing under test is how many requests the viewer starts.
  const jpeg = await (await page.request.get(`${BASE}/iiif/default/0,0,512,512/512,/0/default.jpg`)).body();
  await page.route(
    (url) => url.pathname === "/iiif/default/info.json",
    async (route) => {
      const info = await (await route.fetch()).json();
      const factors = [1, 2, 4, 8, 16];
      info.width = 8192;
      info.height = 8192;
      info.tiles = [{ width: 512, scaleFactors: factors }];
      info.sizes = factors.map((f) => ({ width: 8192 / f, height: 8192 / f }));
      await route.fulfill({ json: info });
    },
  );

  let inFlight = 0;
  let most = 0;
  let served = 0;
  await page.route(isTile, async (route) => {
    inFlight++;
    most = Math.max(most, inFlight);
    await new Promise((resolve) => setTimeout(resolve, 1_500));
    inFlight--;
    served++;
    await route.fulfill({ contentType: "image/jpeg", body: jpeg });
  });

  await page.goto(`${BASE}/viewer/`);
  await waitForTiles(page);

  expect(served, "the view must need more than six tiles, or this proves nothing").toBeGreaterThan(6);
  expect(most, "tile requests outstanding at once").toBeLessThanOrEqual(6);
});
