// The window in every state, every language, light and dark (see the
// projects in playwright.config.ts), with the backend replaced by tests/tauri.ts.
// Each state is checked for its behaviour and for text that is cut off.

import fs from "node:fs";
import path from "node:path";
import { expect, test, type BrowserContext, type Page } from "@playwright/test";
import { LANGUAGES, type Key, type Lang } from "../src/i18n/index.ts";
import { expectTidy } from "./layout.ts";
import { DISKS, REMEMBERED, emit, open } from "./tauri.ts";

const SMALL = { width: 520, height: 320 };
const TALL = { width: 520, height: 480 };
const LARGE = { width: 880, height: 660 };

const summary = (o = {}) => ({
  copyCount: 1911,
  copyBytes: 29.2e9,
  deleteCount: 17,
  renameCount: 2,
  mkdirCount: 3,
  unchangedCount: 482113,
  ...o,
});
const result = (o: Record<string, unknown>) => ({
  mode: "mirror",
  issues: [],
  messages: [],
  details: [],
  summary: summary(),
  durationSecs: 12,
  ...o,
});

/** Planned changes of a scan, sorted by path like the backend sends them. */
function scanItems(years = 6, perAlbum = 40) {
  const items: Record<string, unknown>[] = [];
  for (let y = 0; y < years; y++)
    for (const album of ["Urlaub", "Familie", "Export"])
      for (let i = 0; i < perAlbum; i++)
        items.push({ k: "c", p: `Fotos/${2019 + y}/${album}/IMG_${4700 + i}.CR3`, n: 28e6 + i * 31337 });
  items.push(
    { k: "m", p: "Fotos/2024/Neu", n: 0, dir: true },
    { k: "c", p: "Fotos/2024/Neu/cover.jpg", n: 3.1e6 },
    { k: "d", p: "Fotos/2019/alt", n: 214, dir: true },
    { k: "d", p: "Notizen.txt", n: 1 },
    { k: "x", p: "Projekte/plan", n: 0 },
    { k: "c", p: "Projekte/plan", n: 12000 },
    { k: "r", p: "Projekte/README.md", n: 0, from: "readme.md" },
    { k: "c", p: "Projekte/ein sehr langer Ordnername der nicht in die Zeile passt/eine Datei mit langem Namen.pdf", n: 9e6 },
  );
  return items.sort((a, b) => ((a.p as string) < (b.p as string) ? -1 : (a.p as string) > (b.p as string) ? 1 : 0));
}

const DETAILS = [
  "contents not readable: /Volumes/Fotos-A/Privat (Permission denied (os error 13))",
  ...Array.from({ length: 30 }, (_, i) => `copy Fotos/2023/Export/IMG_${5000 + i}.CR3: No space left on device (os error 28)`),
  "copy Fotos/2024/Video/clip.mov: Input/output error (os error 5)",
];

for (const lang of Object.keys(LANGUAGES) as Lang[]) {
  /** A plain (not plural) text of this language. */
  const text = (key: Key) => LANGUAGES[lang][key] as string;

  test.describe(lang, () => {
    let errors: string[];
    let shots: string | undefined;

    test.beforeEach(async ({}, info) => {
      errors = [];
      const dir = process.env.SCREENSHOTS;
      if (dir) {
        shots = path.join(dir, `${lang}-${info.project.name}`);
        fs.mkdirSync(shots, { recursive: true });
      }
    });
    test.afterEach(() => {
      expect(errors, "JavaScript errors").toEqual([]);
    });

    /** Opens the window in a new page, as if the app had been started. */
    const openWith = async (context: BrowserContext, backend = {}, storage: Record<string, unknown> = REMEMBERED) => {
      const page = await context.newPage();
      await open(page, { locales: [lang], ...backend }, storage, errors);
      return page;
    };
    const tidy = (page: Page, state: string) => expectTidy(page, state, shots);

    test("choosing source and target", async ({ context }) => {
      let page = await openWith(context, {}, {});
      await expect(page.locator("#start")).toBeDisabled();
      await expect(page.locator("#setup-hint")).toHaveText(text("hintChoose"));
      await tidy(page, "01-nothing-chosen");

      page = await openWith(context);
      await expect(page.locator("#start")).toBeEnabled();
      await expect(page.locator("#start")).toHaveText(text("mirror"));
      await expect(page.locator("#setup-hint")).toHaveText(text("hintMirror"));
      await tidy(page, "02-two-disks");

      const long = "Ein sehr langer Name für eine externe Festplatte";
      page = await openWith(
        context,
        { volumes: [{ ...DISKS[0], name: long, path: `/Volumes/${long}` }] },
        {
          source: { path: "/Users/anna/Pictures/Archiv/2024/Hochzeit/RAW-Dateien/Kamera 2", name: "Kamera 2", kind: "folder" },
          target: { path: `/Volumes/${long}`, name: long, kind: "volume", free: 5e9, total: 4e12 },
        },
      );
      await tidy(page, "03-long-names");
    });

    test("a pair that cannot be mirrored", async ({ context }) => {
      let page = await openWith(context, { checkPair: { key: "targetInsideSource" } });
      await expect(page.locator("#start")).toBeDisabled();
      await expect(page.locator("#setup-hint")).not.toHaveText(text("hintMirror"));
      await tidy(page, "04-problem");

      // A remembered disk that is not plugged in.
      page = await openWith(context, { volumes: [DISKS[0]], checkPair: { key: "targetMissing", path: "/Volumes/Fotos-B" } });
      await expect(page.locator("#pick-target")).toHaveClass(/offline/);
      await expect(page.locator("#pick-target .tile-detail")).toHaveText(text("offline"));
      await expect(page.locator("#pick-source")).not.toHaveClass(/offline/);
      await expect(page.locator("#start")).toBeDisabled();
      await tidy(page, "05-disk-not-connected");

      // The backend refuses to start.
      page = await openWith(context, { startError: { key: "runInProgress" } });
      await page.click("#start");
      await expect(page.locator("#setup")).toBeVisible();
      await expect(page.locator("#setup-hint")).not.toHaveText(text("hintMirror"));
      await tidy(page, "06-start-refused");
    });

    test("the Start mode is remembered", async ({ context }) => {
      let page = await openWith(context, {}, { ...REMEMBERED, mode: "scan" });
      await expect(page.locator("#start")).toHaveText(text("scan"));
      await expect(page.locator("#setup-hint")).toHaveText(text("hintScan"));
      await tidy(page, "07-scan-mode");

      // A damaged value falls back to Mirror.
      page = await openWith(context, {}, { ...REMEMBERED, mode: "{broken" });
      await expect(page.locator("#start")).toHaveText(text("mirror"));
    });

    test("a mirror in progress", async ({ context }) => {
      const page = await openWith(context);
      await page.click("#start");
      await expect(page.locator("#running")).toBeVisible();
      await emit(page, "run://phase", "scanning");
      await emit(page, "run://scan-progress", 182345);
      await tidy(page, "08-comparing");

      await emit(page, "run://phase", "copying");
      await emit(page, "run://progress", {
        bytesDone: 1.2e12, bytesTotal: 3.9e12, filesDone: 123456, filesTotal: 987654,
        current: "Fotos/2024/Urlaub mit einem sehr langen Ordnernamen/noch ein Unterordner/IMG_4711.CR3",
        bytesPerSec: 1.1e9, etaSecs: 3 * 3600 + 25 * 60, fraction: 0.31,
      });
      // The bar shows the backend's share of done work.
      expect(await page.locator("#run-bar").evaluate((el) => el.style.width)).toBe("31%");
      await expect(page.locator("#run-current")).toBeVisible();
      await tidy(page, "09-copying");

      // Problems show up during the run, in place of the current file.
      await emit(page, "run://issues", ["targetFull", "noPermission"]);
      await expect(page.locator("#run-issues")).toBeVisible();
      await expect(page.locator("#run-issues")).toContainText(text("issueTargetFull"));
      await expect(page.locator("#run-current")).toBeHidden();
      await tidy(page, "10-copying-with-problems");

      await page.click("#cancel");
      await expect(page.locator("#cancel")).toBeDisabled();
      await expect(page.locator("#run-phase")).toHaveText(text("cancelling"));
      await tidy(page, "11-cancelling");
      expect(await page.evaluate(() => window.__calls.some((c) => c.cmd === "cancel"))).toBe(true);
    });

    test("results", async ({ context }) => {
      const page = await openWith(context);
      const finish = async (r: Record<string, unknown>) => {
        if (await page.locator("#done").isVisible()) await page.click("#done-ok");
        await page.click("#start");
        await emit(page, "run://done", result(r));
        await expect(page.locator("#done")).toBeVisible();
      };

      await finish({ outcome: "success", headline: { key: "mirrored", target: "Fotos-B", source: "Fotos-A" } });
      await expect(page.locator("#done-retry")).toBeHidden();
      await expect(page.locator("#details-toggle")).toBeHidden();
      await expect(page.locator("#done-issues > *")).toHaveCount(0);
      expect(page.viewportSize()).toEqual(SMALL);
      await tidy(page, "12-done");

      await finish({ outcome: "inSync", headline: { key: "inSync" }, summary: summary({ copyCount: 0 }) });
      await expect(page.locator("#done-headline")).toHaveText(text("inSyncTitle"));
      await tidy(page, "13-in-sync");

      // Several kinds of problems: one entry each, the window grows.
      await finish({
        outcome: "incomplete",
        headline: { key: "finishedWithErrors", count: 414 },
        issues: [{ kind: "targetFull", count: 412 }, { kind: "noPermission", count: 1 }, { kind: "other", count: 1 }],
        messages: [{ key: "skippedAfterFailures", count: 17 }],
        details: DETAILS,
        summary: summary({ copyCount: 1498, copyBytes: 21e9, deleteCount: 0 }),
      });
      await expect(page.locator("#done-headline")).toHaveText(text("notAllMirrored"));
      await expect(page.locator("#done-issues .issue")).toHaveCount(3);
      await expect(page.locator("#done-issues .issue").first()).toContainText(text("issueTargetFull"));
      await expect(page.locator("#done-retry")).toHaveText(text("retry"));
      expect(page.viewportSize()).toEqual(TALL);
      await tidy(page, "14-problems");

      // Details replace the list and can be copied.
      await page.click("#details-toggle");
      await expect(page.locator("#done-details")).toBeVisible();
      await expect(page.locator("#done-details")).toContainText(DETAILS[0]);
      await expect(page.locator("#done-issues")).toBeHidden();
      await expect(page.locator("#details-copy")).toBeVisible();
      await tidy(page, "15-details");
      await page.click("#details-toggle");
      await expect(page.locator("#done-issues")).toBeVisible();

      // Mirror Again starts a mirror.
      await page.click("#done-retry");
      await expect(page.locator("#running")).toBeVisible();
      await emit(page, "run://done", result({ outcome: "success", headline: { key: "inSync" } }));

      // One kind of problem fits the small window.
      await finish({
        outcome: "incomplete",
        headline: { key: "finishedIncomplete" },
        issues: [{ kind: "nameClash", count: 2 }],
        details: ['cannot mirror Projekte/Readme.md: also present as "README.md"'],
        summary: summary({ copyCount: 12, copyBytes: 48e6, deleteCount: 0, renameCount: 0 }),
      });
      await expect(page.locator("#done-issues .issue")).toHaveCount(1);
      expect(page.viewportSize()).toEqual(SMALL);
      await tidy(page, "16-name-clash");

      await finish({
        outcome: "incomplete",
        headline: { key: "finishedWithErrors", count: 3 },
        issues: [{ kind: "tooLarge", count: 3 }],
        details: ["copy Filme/a.mkv: File too large (os error 27)"],
      });
      await tidy(page, "17-too-large");

      // Mirroring again cannot help an empty source.
      await finish({ outcome: "failed", headline: { key: "sourceEmpty", entries: 48213 } });
      await expect(page.locator("#done-headline")).toHaveText(text("sourceEmptyTitle"));
      await expect(page.locator("#done-retry")).toBeHidden();
      await tidy(page, "18-source-empty");

      // After a cancel, the next step is to resume.
      await finish({ outcome: "cancelled", headline: { key: "cancelledPartly" } });
      await expect(page.locator("#done-headline")).toHaveText(text("cancelledTitle"));
      await expect(page.locator("#done-retry")).toHaveText(text("resume"));
      await tidy(page, "19-cancelled");

      await page.click("#done-ok");
      await expect(page.locator("#setup")).toBeVisible();
      expect(page.viewportSize()).toEqual(SMALL);
    });

    test("scan results", async ({ context }) => {
      const page = await openWith(context, {}, { ...REMEMBERED, mode: "scan" });
      await page.click("#start");
      const scanned = (items: unknown[], s: Record<string, unknown>, more: Record<string, unknown> = {}) =>
        emit(page, "run://done", result({ mode: "scan", outcome: "scanned", headline: { key: "scanFinished" }, summary: summary(s), items, ...more }));

      await scanned(scanItems(), { copyCount: 722, copyBytes: 20.4e9, deleteCount: 217, renameCount: 1 }, {
        issues: [{ kind: "unreadable", count: 2 }],
        messages: [{ key: "deletionsWillBeSkipped", count: 4, unreadable: 2, collisions: 0 }],
      });
      await expect(page.locator("#scan")).toBeVisible();
      expect(page.viewportSize()).toEqual(LARGE);
      await expect(page.locator("#count-copy")).toHaveText("722");
      await expect(page.locator("#scan-issues")).toBeVisible();
      await expect(page.locator("#scan-mirror")).toBeVisible();
      await tidy(page, "20-scan-tree");

      await page.locator(".row.folder").nth(2).click();
      await tidy(page, "21-scan-tree-expanded");

      // Hiding copies leaves only deletions and renames.
      await page.click('.chip[data-cat="copy"]');
      await expect(page.locator('.chip[data-cat="copy"]')).not.toHaveClass(/\bon\b/);
      await tidy(page, "22-scan-without-copies");
      await page.click('.chip[data-cat="copy"]');

      await page.click("#view-list");
      await expect(page.locator("#view-list")).toHaveClass(/\bon\b/);
      await tidy(page, "23-scan-list");

      await page.fill("#scan-search", "IMG_4711");
      await expect(page.locator(".row").first()).toContainText("IMG_4711");
      await tidy(page, "24-scan-search");
      await page.fill("#scan-search", "nothing like this");
      await expect(page.locator(".row")).toHaveCount(0);
      await tidy(page, "25-scan-no-match");
      await page.fill("#scan-search", "");

      // The smallest size the window can be dragged to.
      await page.setViewportSize({ width: 680, height: 460 });
      await tidy(page, "26-scan-smallest");
      await page.setViewportSize(LARGE);

      // Nothing to do: nothing to mirror either.
      await page.click("#scan-close");
      await page.click("#start");
      await scanned([], { copyCount: 0, copyBytes: 0, deleteCount: 0, renameCount: 0 });
      await expect(page.locator("#scan-mirror")).toBeHidden();
      await tidy(page, "27-scan-nothing-to-do");
    });

  });
}

test("a scan with 100,000 changes only renders the visible rows", async ({ page }, info) => {
  test.skip(info.project.name !== "light", "the colour scheme does not matter here");
  const errors: string[] = [];
  await open(page, { locales: ["en"] }, { ...REMEMBERED, mode: "scan" }, errors);
  await page.click("#start");
  const items = scanItems(60, 560);
  await emit(page, "run://done", result({ mode: "scan", outcome: "scanned", headline: { key: "scanFinished" }, summary: summary({ copyCount: items.length }), items }));
  await page.click("#view-list");
  await expect(page.locator(".row").first()).toBeVisible();
  expect(await page.locator(".row").count()).toBeLessThan(200);

  await page.locator("#scan-rows").evaluate((el) => (el.scrollTop = el.scrollHeight));
  const last = items[items.length - 1].p as string;
  await expect(page.locator(".row").last()).toContainText(path.basename(last));
  expect(await page.locator(".row").count()).toBeLessThan(200);
  expect(errors).toEqual([]);
});
