import { defineConfig, devices } from "@playwright/test";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

/**
 * Browser tests for ziv's built-in viewer.
 *
 * This project is deliberately OUTSIDE the cargo workspace, the same way `fuzz/` is: it needs a
 * Node toolchain and a downloaded browser, neither of which should be a precondition for
 * `cargo build`/`cargo test` at the repo root.
 *
 * The viewer's controls are the one part of ziv whose behaviour no Rust test can reach — they are
 * JavaScript driving OpenSeadragon, and the property that matters ("moving this slider makes the
 * server return the plane it names, and the browser paints it") only exists once a real browser
 * has parsed the page, run the script, fetched tiles over HTTP and rasterised them.
 */

export const REPO_ROOT = resolve(__dirname, "..");
/** The release binary every server below runs, and which `navigation.spec.ts` exports with. */
export const ZIV = resolve(REPO_ROOT, "target/release/ziv");

if (!existsSync(ZIV)) {
  throw new Error(
    `ziv release binary not found at ${ZIV}.\n` +
      `Build it first:  cargo build --release -p ziv`,
  );
}

/** 3-channel, 3 timepoints, 5 z-planes — the image the controls are exercised against. */
export const MULTIDIM_PORT = 3079;
/** Single plane, single channel — the image whose controls should not appear at all. */
export const FLAT_PORT = 3078;
/** Carries a `labels/` group — the only fixture whose label controls exist. */
export const LABELS_PORT = 3077;
/** Two images from one process, so the root viewer shows a picker rather than an image. */
export const CATALOGUE_PORT = 3076;

function serve(fixture: string, port: number) {
  return {
    command: `${JSON.stringify(ZIV)} serve ${JSON.stringify(
      resolve(REPO_ROOT, fixture),
    )} --addr 127.0.0.1:${port}`,
    // `/ziv/dimensions.json` rather than the viewer page: it is the last thing to become
    // available, so waiting on it means the first test cannot race startup.
    url: `http://127.0.0.1:${port}/ziv/dimensions.json`,
    reuseExistingServer: !process.env.CI,
    stdout: "pipe" as const,
    stderr: "pipe" as const,
    timeout: 120_000,
  };
}

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? [["github"], ["html", { open: "never" }]] : [["list"]],
  timeout: 60_000,
  expect: { timeout: 15_000 },
  use: {
    baseURL: `http://127.0.0.1:${MULTIDIM_PORT}`,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: [
    serve("tests/fixtures/sample_multidim.ome.zarr", MULTIDIM_PORT),
    serve("tests/fixtures/sample_multi_tile.ome.zarr", FLAT_PORT),
    serve("tests/fixtures/sample_labels.ome.zarr", LABELS_PORT),
    {
      command: `${JSON.stringify(ZIV)} serve ${JSON.stringify(
        resolve(REPO_ROOT, "tests/fixtures/sample_multidim.ome.zarr"),
      )} ${JSON.stringify(
        resolve(REPO_ROOT, "tests/fixtures/sample_labels.ome.zarr"),
      )} --addr 127.0.0.1:${CATALOGUE_PORT}`,
      // The catalogue is the last thing to become available on a multi-image server, so waiting
      // on it means the first test cannot race startup.
      url: `http://127.0.0.1:${CATALOGUE_PORT}/ziv/images.json`,
      reuseExistingServer: !process.env.CI,
      stdout: "pipe" as const,
      stderr: "pipe" as const,
      timeout: 120_000,
    },
  ],
});
