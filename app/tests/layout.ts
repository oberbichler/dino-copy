import { expect, type Page } from "@playwright/test";

/**
 * Elements whose text is cut off or that stick out of the window, except where
 * an ellipsis or a scrolling area is intended. Returns one line per problem.
 */
export async function layoutProblems(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const problems: string[] = [];
    const vw = innerWidth;
    const vh = innerHeight;
    for (const el of document.querySelectorAll<HTMLElement>("body *")) {
      if (el.closest("[hidden]") || el.closest(".rows")) continue;
      // Clipped on purpose: the progress bar, and texts that end in an ellipsis.
      if (el.parentElement?.closest(".bar, .current, .tile-detail, .stat-value, .tile-name")) continue;
      const cs = getComputedStyle(el);
      if (cs.display === "none" || cs.visibility === "hidden") continue;
      const r = el.getBoundingClientRect();
      if (r.width === 0 && r.height === 0) continue;
      const name = el.id ? `#${el.id}` : `${el.tagName.toLowerCase()}.${[...el.classList].join(".")}`;
      if (r.right > vw + 0.5 || r.bottom > vh + 0.5 || r.left < -0.5 || r.top < -0.5) {
        problems.push(`${name} sticks out of the window`);
      }
      const ellipsis = cs.textOverflow === "ellipsis";
      const scrolls = [cs.overflowX, cs.overflowY].some((o) => o === "auto" || o === "scroll");
      if (!ellipsis && !scrolls && cs.overflowX !== "visible" && el.scrollWidth > el.clientWidth + 1) {
        problems.push(`${name} is cut off at the side`);
      }
      if (!scrolls && cs.overflowY === "hidden" && el.children.length === 0 && el.scrollHeight > el.clientHeight + 1) {
        problems.push(`${name} is cut off at the bottom`);
      }
      if (el.tagName === "BUTTON" && el.scrollWidth > el.clientWidth + 1) {
        problems.push(`${name}: the label does not fit`);
      }
    }
    return problems;
  });
}

/**
 * Checks that nothing is cut off. With SCREENSHOTS=<dir>, also saves a
 * screenshot of the state for a look by eye.
 */
export async function expectTidy(page: Page, state: string, shots?: string): Promise<void> {
  // Let transitions end and the layout settle after a resize.
  await page.evaluate(async () => {
    const finite = document.getAnimations().filter((a) => a.effect?.getTiming().iterations !== Infinity);
    await Promise.all(finite.map((a) => a.finished.catch(() => undefined)));
    await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
  });
  if (shots) await page.screenshot({ path: `${shots}/${state}.png` });
  expect.soft(await layoutProblems(page), `layout of "${state}"`).toEqual([]);
}
