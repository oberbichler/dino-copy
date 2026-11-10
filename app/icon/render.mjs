// Renders icon.svg to a 1024 px PNG with a transparent background.
// usage: node render.mjs icon.svg out.png
import { webkit } from "playwright";
import fs from "node:fs";
const [src, out] = process.argv.slice(2);
const svg = fs.readFileSync(src, "utf8");
const browser = await webkit.launch();
const page = await browser.newPage({ viewport: { width: 1024, height: 1024 } });
await page.setContent(`<html><body style="margin:0;background:transparent">${svg.replace("<svg ", '<svg width="1024" height="1024" ')}</body></html>`);
await page.screenshot({ path: out, omitBackground: true });
await browser.close();
