import { test, expect } from "@playwright/test";

const nodeCount = (text: string | null): number => {
  const m = text?.match(/(\d+)\s+nodes/);
  return m ? parseInt(m[1], 10) : -1;
};

test("renders the graph and toggles crate/file views without console errors", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(e.message));

  await page.goto("/");

  // WebGL canvas mounted with a real size.
  const canvas = page.locator("canvas");
  await expect(canvas).toBeVisible();
  const box = await canvas.boundingBox();
  expect(box, "canvas has a bounding box").not.toBeNull();
  expect(box!.width).toBeGreaterThan(100);
  expect(box!.height).toBeGreaterThan(100);

  // Wait for the engine's first rows to sync into the store (counts > 0).
  const counts = page.locator(".counts");
  await expect
    .poll(async () => nodeCount(await counts.textContent()), { timeout: 60_000 })
    .toBeGreaterThan(0);
  const fileNodes = nodeCount(await counts.textContent());

  // The FPS meter is live (frames are flowing from the worker's positions).
  await expect(page.locator(".perf")).toContainText("fps");

  // Files is the default tab.
  await expect(page.getByRole("button", { name: "Files" })).toHaveClass(/on/);

  // Let the initial layout settle before switching tabs (switching reuses cached
  // positions without recomputing, so we want a good layout cached first).
  await page.waitForTimeout(2500);

  // Modules view has fewer nodes (one per module, not per file).
  await page.getByRole("button", { name: "Modules" }).click();
  await expect(page.getByRole("button", { name: "Modules" })).toHaveClass(/on/);
  await expect
    .poll(async () => nodeCount(await counts.textContent()), { timeout: 60_000 })
    .toBeLessThan(fileNodes);

  // Back to Files returns to the original count.
  await page.getByRole("button", { name: "Files" }).click();
  await expect
    .poll(async () => nodeCount(await counts.textContent()), { timeout: 60_000 })
    .toBe(fileNodes);

  // Pause toggles status text.
  await page.getByRole("button", { name: "Pause" }).click();
  await expect(page.locator(".status")).toContainText("paused");

  // Capture the (default) file view once settled.
  await page.getByRole("button", { name: "Resume" }).click();
  await page.waitForTimeout(3000);
  await page.screenshot({ path: "test-results/graph.png" });

  expect(errors, `console errors:\n${errors.join("\n")}`).toEqual([]);
});
