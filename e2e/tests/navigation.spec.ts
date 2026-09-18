import { test, expect, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { FLAT_PORT, REPO_ROOT, ZIV } from "../playwright.config";
import { serveExport, waitForTiles } from "./helpers";

/**
 * The zoom in, zoom out, home and full-screen buttons drawn by `assets/viewer/nav/nav.js`.
 *
 * They replace OpenSeadragon's image-sprite buttons in BOTH viewers ziv ships: the live one `serve`
 * embeds, and the one `export` writes into every static tree. The two are the same file, distinguished
 * by a `<meta name="ziv-mode">` marker, so the loop below runs every test against both modes rather
 * than against two separate pages.
 *
 * The flat 1024x1024 fixture is used rather than the 32x32 multidim one: at 32 pixels the home view
 * is already past OpenSeadragon's maximum zoom, so "zoom in" would have nothing to do.
 */

/** A hostname that cannot resolve, so the export is only reachable through `page.route` below. */
const EXPORT_ORIGIN = "http://ziv-export.test";

let exportDir = "";

test.beforeAll(() => {
  exportDir = mkdtempSync(join(tmpdir(), "ziv-nav-export-"));
  execFileSync(ZIV, [
    "export",
    resolve(REPO_ROOT, "tests/fixtures/sample_multi_tile.ome.zarr"),
    exportDir,
  ]);
});

test.afterAll(() => {
  if (exportDir) rmSync(exportDir, { recursive: true, force: true });
});

const VIEWERS = [
  {
    name: "live viewer",
    open: (page: Page) => page.goto(`http://127.0.0.1:${FLAT_PORT}/viewer/`),
  },
  {
    name: "static export",
    open: async (page: Page) => {
      await serveExport(page, EXPORT_ORIGIN, exportDir);
      await page.goto(`${EXPORT_ORIGIN}/index.html`);
    },
  },
];

/**
 * OpenSeadragon's viewport, read at its TARGET rather than mid-animation. A click starts a zoom
 * animation, and the target is where it will land, so a test does not have to wait it out.
 */
async function view(page: Page) {
  return page.evaluate(() => {
    const v = (window as any).OpenSeadragon.getViewer(document.getElementById("osd"));
    const vp = v.viewport;
    const centre = vp.getCenter(false);
    const home = vp.getHomeBounds().getCenter();
    return {
      zoom: vp.getZoom(false),
      min: vp.getMinZoom(),
      max: vp.getMaxZoom(),
      home: vp.getHomeZoom(),
      perClick: v.zoomPerClick,
      cx: centre.x,
      cy: centre.y,
      hx: home.x,
      hy: home.y,
    };
  });
}

async function isFullPage(page: Page): Promise<boolean> {
  return page.evaluate(() =>
    (window as any).OpenSeadragon.getViewer(document.getElementById("osd")).isFullPage(),
  );
}

function near(a: number, b: number) {
  return Math.abs(a - b) < 1e-6;
}

/** Tabs forward until the named button has focus, so the test proves it is in the tab order. */
async function tabTo(page: Page, label: string) {
  for (let i = 0; i < 12; i++) {
    const focused = await page.evaluate(() => document.activeElement?.getAttribute("aria-label"));
    if (focused === label) return;
    await page.keyboard.press("Tab");
  }
}

for (const viewer of VIEWERS) {
  test.describe(viewer.name, () => {
    test.beforeEach(async ({ page }) => {
      await viewer.open(page);
      await waitForTiles(page);
    });

    test("draws one set of buttons and none of OpenSeadragon's sprites", async ({ page }) => {
      for (const name of ["Zoom in", "Zoom out", "Go home", "Full screen"]) {
        await expect(page.getByRole("button", { name, exact: true })).toHaveCount(1);
        await expect(page.getByRole("button", { name, exact: true })).toBeVisible();
      }
      // With the sprites switched off OpenSeadragon never asks for its button images. A request
      // for one means both sets are being built, with ours drawn over the top.
      const sprites = await page.evaluate(() =>
        performance
          .getEntriesByType("resource")
          .map((e) => e.name)
          .filter((n) => /\/(openseadragon|osd)\/images\//.test(n)),
      );
      expect(sprites).toEqual([]);
    });

    test("zoom in and zoom out step by zoomPerClick, within the zoom limits", async ({ page }) => {
      // Exactly what OpenSeadragon's own buttons do: multiply by zoomPerClick, then clamp.
      const start = await view(page);
      await page.getByRole("button", { name: "Zoom in", exact: true }).click();
      const wantIn = Math.min(start.zoom * start.perClick, start.max);
      await expect.poll(async () => near((await view(page)).zoom, wantIn)).toBe(true);
      expect(wantIn).toBeGreaterThan(start.zoom);

      await page.getByRole("button", { name: "Zoom out", exact: true }).click();
      const wantOut = Math.max(wantIn / start.perClick, start.min);
      await expect.poll(async () => near((await view(page)).zoom, wantOut)).toBe(true);
      expect(wantOut).toBeLessThan(wantIn);
    });

    test("home returns to the home view", async ({ page }) => {
      await page.evaluate(() => {
        const w = window as any;
        const v = w.OpenSeadragon.getViewer(document.getElementById("osd"));
        v.viewport.zoomTo(v.viewport.getMaxZoom(), null, true);
        v.viewport.panTo(new w.OpenSeadragon.Point(0.2, 0.3), true);
        v.viewport.applyConstraints(true);
      });
      const away = await view(page);
      expect(near(away.zoom, away.home)).toBe(false);

      await page.getByRole("button", { name: "Go home", exact: true }).click();
      await expect
        .poll(async () => {
          const v = await view(page);
          return near(v.zoom, v.home) && near(v.cx, v.hx) && near(v.cy, v.hy);
        })
        .toBe(true);
    });

    test("full screen toggles, and the icon and label follow it", async ({ page }) => {
      await page.getByRole("button", { name: "Full screen", exact: true }).click();
      await expect.poll(() => isFullPage(page)).toBe(true);

      const exit = page.getByRole("button", { name: "Exit full screen", exact: true });
      await expect(exit).toHaveAttribute("aria-pressed", "true");
      await expect(exit.locator("svg")).toHaveAttribute("data-icon", "minimize");
      // Full page detaches everything outside the viewer. The buttons are an OpenSeadragon control
      // precisely so that they are still here to get back out with.
      await expect(exit).toBeVisible();

      await exit.click();
      await expect.poll(() => isFullPage(page)).toBe(false);
      const enter = page.getByRole("button", { name: "Full screen", exact: true });
      await expect(enter).toHaveAttribute("aria-pressed", "false");
      await expect(enter.locator("svg")).toHaveAttribute("data-icon", "maximize");
    });

    test("leaving full screen without the button still resets it", async ({ page }) => {
      // Escape belongs to the browser rather than the page, so leave the way Escape does, through
      // the Fullscreen API. The button follows OpenSeadragon's events rather than its own clicks
      // for exactly this case.
      await page.getByRole("button", { name: "Full screen", exact: true }).click();
      await expect.poll(() => isFullPage(page)).toBe(true);
      // Checked first: the button starts unpressed, so without this the assertions below would pass
      // for a button that never changed at all.
      await expect(
        page.getByRole("button", { name: "Exit full screen", exact: true }),
      ).toHaveAttribute("aria-pressed", "true");

      await page.evaluate(() => document.exitFullscreen());
      await expect.poll(() => isFullPage(page)).toBe(false);
      const enter = page.getByRole("button", { name: "Full screen", exact: true });
      await expect(enter).toHaveAttribute("aria-pressed", "false");
      await expect(enter.locator("svg")).toHaveAttribute("data-icon", "maximize");
    });

    test("the buttons can be reached and used from the keyboard", async ({ page }) => {
      await tabTo(page, "Zoom in");
      const zoomIn = page.getByRole("button", { name: "Zoom in", exact: true });
      await expect(zoomIn).toBeFocused();
      // Focus has to be visible to be of any use to someone navigating by keyboard.
      expect(await zoomIn.evaluate((el) => getComputedStyle(el).outlineStyle)).toBe("solid");

      const before = await view(page);
      await page.keyboard.press("Enter");
      await expect.poll(async () => (await view(page)).zoom).toBeGreaterThan(before.zoom);

      await page.keyboard.press("Tab");
      await expect(page.getByRole("button", { name: "Zoom out", exact: true })).toBeFocused();
      const zoomed = await view(page);
      await page.keyboard.press("Space");
      await expect.poll(async () => (await view(page)).zoom).toBeLessThan(zoomed.zoom);
    });

    test("the buttons do not fade out when the pointer leaves", async ({ page }) => {
      // OpenSeadragon fades its controls 2s after the pointer leaves the viewer, over another 1.5s.
      // The control panel never fades, so neither do these.
      //
      // The viewer fills the page, so there is nowhere outside it for the pointer to go. Shorten it
      // to make somewhere: without a real leave OpenSeadragon never starts the fade, and this test
      // would pass with fading switched back on.
      await page.evaluate(() => {
        (document.getElementById("osd") as HTMLElement).style.height = "50%";
      });
      const size = page.viewportSize()!;
      await page.mouse.move(size.width / 2, size.height * 0.25);
      await page.mouse.move(size.width / 2, size.height * 0.9);
      await page.waitForTimeout(4_000);
      // What the user sees is the product of every ancestor's opacity, and OpenSeadragon fades a
      // wrapper it puts around the control rather than the control itself.
      const opacity = await page
        .getByRole("group", { name: "Image navigation" })
        .evaluate((el) => {
          let product = 1;
          for (let n: Element | null = el; n; n = n.parentElement) {
            product *= Number(getComputedStyle(n).opacity);
          }
          return product;
        });
      expect(opacity).toBe(1);
    });

    test("loads with a clean console", async ({ page }) => {
      const errors: string[] = [];
      page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
      page.on("pageerror", (e) => errors.push(String(e)));
      await page.reload();
      await waitForTiles(page);
      expect(errors).toEqual([]);
    });
  });
}
