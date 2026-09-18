import { expect, type Page } from "@playwright/test";
import { existsSync, statSync } from "node:fs";
import { join, normalize, sep } from "node:path";

/**
 * Shared browser-side helpers for the viewer specs.
 *
 * They live here rather than in one spec file because every spec needs the same answer to the same
 * hard question: "is the viewer actually showing what I just asked for, or is it still showing the
 * previous thing?" Getting that wrong does not fail a test, it passes one — see `showing`.
 */

/**
 * Serves a directory at `origin` through `page.route`, the way a static host would, with no server
 * process. `origin` should be a hostname that cannot resolve (e.g. `http://ziv-export.test`), so the
 * export is reachable only through this route.
 */
export async function serveExport(page: Page, origin: string, dir: string) {
  await page.route(`${origin}/**`, async (route) => {
    const path = decodeURIComponent(new URL(route.request().url()).pathname);
    const file = normalize(join(dir, path));
    if (!file.startsWith(dir + sep) || !existsSync(file) || statSync(file).isDirectory()) {
      return route.fulfill({ status: 404, body: "404" });
    }
    return route.fulfill({ path: file });
  });
}

/** Resolves once OpenSeadragon has an open tile source with every visible tile rasterised. */
export async function waitForTiles(page: Page) {
  await page.waitForFunction(() => {
    const w = window as any;
    if (!w.OpenSeadragon) return false;
    const v = w.OpenSeadragon.getViewer(document.getElementById("osd"));
    if (!v || !v.isOpen() || v.world.getItemCount() === 0) return false;
    return v.world.getItemAt(0).getFullyLoaded();
  });
}

/**
 * Waits until the viewer is actually SHOWING `expected`, and asserts it.
 *
 * Waiting on tiles alone is not enough and silently passes on stale state: a control change queues
 * the reopen on the next animation frame, so for a moment the previous source is still open and
 * still fully loaded. This instead requires three things to agree — the identifier readout, the
 * tile source OpenSeadragon actually has open, and that source being rasterised — which also makes
 * every call an assertion that the readout is not merely cosmetic.
 */
export async function showing(page: Page, expected: string) {
  await expect
    .poll(
      async () =>
        page.evaluate((want) => {
          const w = window as any;
          if (!w.OpenSeadragon) return "no-osd";
          const v = w.OpenSeadragon.getViewer(document.getElementById("osd"));
          if (!v || !v.isOpen() || v.world.getItemCount() === 0) return "not-open";
          const readout = (
            document.getElementById("identifier")?.textContent || ""
          )
            .replace(/^id\s*/, "")
            .trim();
          if (readout !== want) return `readout:${readout}`;
          const item = v.world.getItemAt(0);
          const id = decodeURIComponent(item.source.id || "");
          if (!id.endsWith("/iiif/" + want)) return `source:${id.split("/iiif/")[1]}`;
          return item.getFullyLoaded() ? want : "loading";
        }, expected),
      { message: `viewer should be showing ${expected}`, timeout: 20_000 },
    )
    .toBe(expected);
}

/**
 * The rendered colour at a fractional position of the IMAGE, not of the canvas.
 *
 * The image is letterboxed inside the canvas, so `(0.25, 0.25)` of the canvas is not `(0.25, 0.25)`
 * of the image. This asks OpenSeadragon where the image actually landed and samples there, which
 * is what lets a test talk about "the top-right quadrant" and mean it.
 */
export async function imagePixel(
  page: Page,
  fx: number,
  fy: number,
): Promise<number[]> {
  return page.evaluate(
    ({ fx, fy }) => {
      const w = window as any;
      const v = w.OpenSeadragon.getViewer(document.getElementById("osd"));
      const item = v.world.getItemAt(0);
      const pt = item.imageToViewerElementCoordinates(
        new w.OpenSeadragon.Point(item.source.width * fx, item.source.height * fy),
      );
      const c = document.querySelector("#osd canvas") as HTMLCanvasElement;
      const scale = c.width / c.getBoundingClientRect().width;
      const ctx = c.getContext("2d", { willReadFrequently: true })!;
      const d = ctx.getImageData(
        Math.round(pt.x * scale),
        Math.round(pt.y * scale),
        1,
        1,
      ).data;
      return [d[0], d[1], d[2]];
    },
    { fx, fy },
  );
}

export async function identifier(page: Page): Promise<string> {
  return (await page.locator("#identifier").textContent())!.replace(/^id\s*/, "");
}

/**
 * Clicks the panel's "Copy URL" button and returns exactly the string the viewer attempted to
 * copy, regardless of which path it used: `navigator.clipboard.writeText` on a secure origin
 * (`https:`, or a browser's own allowance for `localhost`/`127.0.0.1`), or the legacy
 * `document.execCommand("copy")` fallback everywhere else. A static export's own `http://` origin
 * is neither, so it actually gets the fallback.
 *
 * Both are stubbed rather than reading the OS clipboard back afterwards: only one of them ever
 * fires for a given page (the viewer's own `copyToClipboard` picks one), but stubbing both once
 * means this helper does not need to know which, and it makes the assertion independent of the
 * browser's real clipboard permissions, which is what keeps it reliable under CI.
 */
export async function copyViaButton(page: Page): Promise<string> {
  await page.evaluate(() => {
    const w = window as any;
    w.__zivCopied = null;
    if (w.navigator.clipboard) {
      w.navigator.clipboard.writeText = (text: string) => {
        w.__zivCopied = text;
        return Promise.resolve();
      };
    }
    (document as any).execCommand = (command: string) => {
      if (command === "copy") {
        w.__zivCopied = (document.activeElement as HTMLTextAreaElement | null)?.value ?? null;
      }
      return true;
    };
  });
  await page.locator("#copy-url").click();
  await expect
    .poll(() => page.evaluate(() => (window as any).__zivCopied))
    .not.toBeNull();
  return page.evaluate(() => (window as any).__zivCopied as string);
}
