// Standalone GPU fps probe: launches a HEADED Chromium that uses the real Mac
// GPU (headless uses SwiftShader, which is software and unrepresentative).
import { chromium } from "@playwright/test";

const URL = process.env.URL || "http://localhost:5173/";
const fpsOf = (s) => +(s?.match(/(\d+)\s+fps/)?.[1] ?? -1);
const nc = (t) => +(t?.match(/(\d+)\s+nodes/)?.[1] ?? -1);

const browser = await chromium.launch({
  headless: false,
  args: ["--ignore-gpu-blocklist", "--enable-gpu-rasterization", "--use-angle=metal"],
});
const page = await browser.newPage();
await page.setViewportSize({ width: 1600, height: 1000 });
await page.goto(URL);
const counts = page.getByTestId("counts");
const perf = page.getByTestId("perf");
for (let i = 0; i < 180; i++) { if (nc(await counts.textContent()) > 0) break; await page.waitForTimeout(500); }
const renderer = await page.evaluate(() => {
  const c = document.createElement("canvas");
  const gl = c.getContext("webgl2") || c.getContext("webgl");
  const dbg = gl.getExtension("WEBGL_debug_renderer_info");
  return dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : "?";
});
console.log("RENDERER", renderer);
const n = nc(await counts.textContent());

// 1) During initial layout settling (worst case CPU+upload churn).
const settle = [];
for (let i = 0; i < 10; i++) { await page.waitForTimeout(400); settle.push(fpsOf(await perf.textContent())); }

// 2) Steady state.
await page.waitForTimeout(6000);
const steady = [];
for (let i = 0; i < 8; i++) { await page.waitForTimeout(400); steady.push(fpsOf(await perf.textContent())); }

// 3) During continuous pan (drag on empty canvas background).
const box = await page.locator("canvas").boundingBox();
const cx = box.x + box.width / 2, cy = box.y + box.height / 2;
await page.mouse.move(cx, cy);
await page.mouse.down();
const drag = [];
for (let i = 0; i < 10; i++) {
  await page.mouse.move(cx + Math.sin(i) * 200, cy + Math.cos(i) * 150, { steps: 4 });
  await page.waitForTimeout(400);
  drag.push(fpsOf(await perf.textContent()));
}
await page.mouse.up();

// 4) Module view.
await page.getByRole("button", { name: "Modules" }).click().catch(() => {});
await page.waitForTimeout(7000);
const modview = [];
for (let i = 0; i < 8; i++) { await page.waitForTimeout(400); modview.push(fpsOf(await perf.textContent())); }

console.log(`GPU-FPS nodes=${n}`);
console.log(`  settle   ${JSON.stringify(settle)}`);
console.log(`  steady   ${JSON.stringify(steady)}`);
console.log(`  pan      ${JSON.stringify(drag)}`);
console.log(`  modules  ${JSON.stringify(modview)}`);
await browser.close();
