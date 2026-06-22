import { test, expect, Page } from "@playwright/test";

const nodeCount = (text: string | null): number => {
  const m = text?.match(/(\d+)\s+nodes/);
  return m ? parseInt(m[1], 10) : -1;
};

// WebGL nodes aren't in the DOM; ForceGraph exposes window.__graph so a test can
// read a node's screen position. Find one in a region clear of the HUD bars.
const findClickableNode = (page: Page) =>
  page.evaluate(() => {
    const g = (window as unknown as { __graph?: { count(): number; nodeScreenPos(i: number): { id: string; x: number; y: number } | null } }).__graph;
    if (!g) return null;
    const W = window.innerWidth;
    const H = window.innerHeight;
    for (let i = 0; i < g.count(); i++) {
      const p = g.nodeScreenPos(i);
      if (p && p.x > 240 && p.x < W - 340 && p.y > 90 && p.y < H - 140) return p;
    }
    return null;
  });

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
  const counts = page.getByTestId("counts");
  await expect
    .poll(async () => nodeCount(await counts.textContent()), { timeout: 60_000 })
    .toBeGreaterThan(0);
  const fileNodes = nodeCount(await counts.textContent());

  // The FPS meter is live (frames are flowing from the worker's positions).
  await expect(page.getByTestId("perf")).toContainText("fps");

  // Files is the default tab.
  await expect(page.getByRole("button", { name: "Files" })).toHaveAttribute("aria-pressed", "true");

  // Let the initial layout settle before switching tabs (switching reuses cached
  // positions without recomputing, so we want a good layout cached first).
  await page.waitForTimeout(2500);

  // Modules view has fewer nodes (one per module, not per file).
  await page.getByRole("button", { name: "Modules" }).click();
  await expect(page.getByRole("button", { name: "Modules" })).toHaveAttribute("aria-pressed", "true");
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
  await expect(page.getByTestId("status")).toContainText("paused");

  // Capture the (default) file view once settled.
  await page.getByRole("button", { name: "Resume" }).click();
  await page.waitForTimeout(3000);
  await page.screenshot({ path: "test-results/graph.png" });

  expect(errors, `console errors:\n${errors.join("\n")}`).toEqual([]);
});

test("clicking a node selects it, and selection keeps working after an interrupted gesture", async ({ page }) => {
  await page.goto("/");
  const counts = page.getByTestId("counts");
  await expect.poll(async () => nodeCount(await counts.textContent()), { timeout: 60_000 }).toBeGreaterThan(0);

  // Module view: fewer, larger nodes — robust to click.
  await page.getByRole("button", { name: "Modules" }).click();
  await page.waitForTimeout(3500); // let the layout settle so positions are stable
  const info = page.getByTestId("info");

  // Click a real node -> info panel appears.
  const node = await findClickableNode(page);
  expect(node, "a node was locatable away from the HUD").not.toBeNull();
  await page.mouse.click(node!.x, node!.y);
  await expect(info).toBeVisible();

  // Click empty space -> info panel dismisses.
  await page.mouse.click(5, Math.round(page.viewportSize()!.height / 2));
  await expect(info).toBeHidden();

  // Simulate an interrupted gesture (a pointercancel that fires instead of
  // pointerup — the bug that used to strand pointer state and break clicking).
  await page.evaluate(() => {
    const el = document.querySelector("canvas")!;
    const opts = { pointerId: 77, pointerType: "touch", clientX: 10, clientY: 10, bubbles: true };
    el.dispatchEvent(new PointerEvent("pointerdown", opts));
    el.dispatchEvent(new PointerEvent("pointercancel", opts));
  });

  // Clicking a node must still select it.
  const node2 = await findClickableNode(page);
  expect(node2).not.toBeNull();
  await page.mouse.click(node2!.x, node2!.y);
  await expect(info).toBeVisible();
});

test("data view lists relations and shows rows in a sortable, filterable table", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(e.message));

  await page.goto("/");
  // Wait until the engine has synced (graph counts > 0), then switch to Data.
  await expect
    .poll(async () => {
      const m = (await page.getByTestId("counts").textContent())?.match(/(\d+)\s+nodes/);
      return m ? parseInt(m[1], 10) : 0;
    }, { timeout: 60_000 })
    .toBeGreaterThan(0);

  await page.getByRole("button", { name: "Data" }).click();

  // The relation rail lists relations (file_node is one of them).
  const rail = page.getByTestId("relation-list");
  await expect(rail).toBeVisible();
  await expect(rail.getByRole("button", { name: /file_node/ })).toBeVisible();

  // Selecting a relation renders rows in the table.
  await rail.getByRole("button", { name: /file_node/ }).click();
  const table = page.getByTestId("data-table");
  await expect.poll(async () => table.locator("tbody tr").count(), { timeout: 30_000 }).toBeGreaterThan(0);
  const total = await table.locator("tbody tr").count();

  // Filtering narrows the rows.
  await page.getByTestId("data-filter").fill(".rs");
  await expect.poll(async () => table.locator("tbody tr").count()).toBeLessThanOrEqual(total);

  // Sorting by a header keeps the table populated.
  await page.getByTestId("data-filter").fill("");
  await table.locator("thead th").first().click();
  await expect(table.locator("tbody tr").first()).toBeVisible();

  // Back to the graph.
  await page.getByRole("button", { name: "Graph" }).click();
  await expect(page.locator("canvas")).toBeVisible();

  expect(errors, `console errors:\n${errors.join("\n")}`).toEqual([]);
});

test("rules view shows the loaded program and finds within it", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(e.message));

  await page.goto("/");
  await page.getByRole("button", { name: "Rules" }).click();

  // The source loads and contains the program's rules.
  const source = page.getByTestId("rules-source");
  await expect(source).toContainText(":-", { timeout: 30_000 });
  await expect(source).toContainText("module_edge");
  await expect(page.getByTestId("rules-stats")).toContainText(/\d+ rules/);

  // Find highlights matches.
  await page.getByTestId("rules-find").fill("file_link");
  await expect(page.getByTestId("rules-stats")).toContainText(/lines match/);
  await expect(source.locator("mark").first()).toBeVisible();

  await page.getByRole("button", { name: "Graph" }).click();
  await expect(page.locator("canvas")).toBeVisible();

  expect(errors, `console errors:\n${errors.join("\n")}`).toEqual([]);
});
