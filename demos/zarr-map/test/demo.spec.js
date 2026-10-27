import { test, expect } from "@playwright/test";

async function ready(page) {
  await page.goto("/");
  await expect(page.locator("#status")).toHaveText(
    /DuckDB v2\.0\.0(?:-dev\d+)?; zarr extension loaded\./,
    { timeout: 30000 },
  );
}

async function run(page, sql) {
  await page.locator("#sql").fill(sql);
  await page.getByRole("button", { name: "Run", exact: true }).click();
}

async function expectMapPoints(page) {
  // Read the real WebGL framebuffer in an animation frame. A canvas element
  // alone does not prove the layer rendered rather than failing silently.
  await expect.poll(() => page.evaluate(() => new Promise((resolve) => {
    requestAnimationFrame(() => {
      const canvas = document.querySelector("canvas");
      const gl = canvas.getContext("webgl2");
      const pixels = new Uint8Array(canvas.width * canvas.height * 4);
      gl.readPixels(0, 0, canvas.width, canvas.height, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
      let colored = 0;
      for (let i = 0; i < pixels.length; i += 4) {
        if (pixels[i] > 20 && pixels[i + 1] > 20 && pixels[i + 2] > 20) colored++;
      }
      resolve(colored);
    });
  }))).toBeGreaterThan(100);
}

test("the default remote query prunes time before fetching chunks", async ({ page }) => {
  await ready(page);
  await expect(page.locator("#sql")).toHaveValue(/bounds\s*:?=/);
});

test("real sample data queries and renders colored points", async ({ page }) => {
  await ready(page);
  const url = new URL("/data/air.zarr", page.url()).href;
  await page.locator("#url").fill(url);
  await run(page, `SELECT lon, lat, air AS value
FROM read_zarr('{url}', dims := ['time','lat','lon'])
WHERE time = TIMESTAMP '2013-01-01'`);
  await expect(page.locator("#status")).toHaveText(
    /1325 rows in .*value 227\.00…302\.60/,
  );

  await expectMapPoints(page);
});

test("missing result columns produce a useful error", async ({ page }) => {
  await ready(page);
  await run(page, "SELECT 1 AS unexpected");
  await expect(page.locator("#status")).toHaveText(
    "Query must return lon, lat and value columns.",
  );
});

test("an empty result is not shown as an infinite value range", async ({ page }) => {
  await ready(page);
  await run(page, "SELECT 0 AS lon, 0 AS lat, 0 AS value WHERE false");
  await expect(page.locator("#status")).toHaveText("0 rows; no data to display.");
});

test("missing extension disables Run and gives the right setup hint", async ({ page }) => {
  await page.route("**/zarr.duckdb_extension.wasm", (route) => route.fulfill({ status: 404 }));
  await page.goto("/");
  await expect(page.locator("#status")).toContainText("wasm_eh", { timeout: 30000 });
  await expect(page.getByRole("button", { name: "Run", exact: true })).toBeDisabled();
});

test("@remote the default ARCO-ERA5 query returns a map", async ({ page }) => {
  test.setTimeout(150000);
  const dataChunks = new Set();
  page.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if (request.method() === "GET" && /\/2m_temperature\/\d+\.0\.0$/.test(path)) {
      dataChunks.add(path);
    }
  });
  await ready(page);
  await page.getByRole("button", { name: "Run", exact: true }).click();
  await expect(page.locator("#status")).not.toHaveText("Running…", { timeout: 120000 });
  await expect(page.locator("#status")).toHaveText(/16380 rows in .*value /);
  expect(dataChunks.size).toBe(1);
  await expectMapPoints(page);
  console.log("Remote query:", await page.locator("#status").textContent());
});
