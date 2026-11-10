// A stand-in for the Tauri bridge, so the window can be tested in a plain
// browser. The backend is replaced by `window.__backend` (set per test), and
// events are sent with `window.__emit`. Resizing the window resizes the page.

import type { Page } from "@playwright/test";

export interface Backend {
  /** The system languages, most preferred first. */
  locales: string[];
  /** What `list_volumes` returns. */
  volumes?: unknown[];
  /** What `check_pair` returns: a message, or null when the pair is fine. */
  checkPair?: unknown;
  /** Thrown by `start`. */
  startError?: unknown;
}

declare global {
  interface Window {
    __backend: Backend;
    __emit(event: string, payload: unknown): void;
    __resize(width: number, height: number): Promise<void>;
    __calls: { cmd: string; args: unknown }[];
  }
}

export const DISKS = [
  { name: "Fotos-A", path: "/Volumes/Fotos-A", totalBytes: 2e12, availableBytes: 412e9, removable: true },
  { name: "Fotos-B", path: "/Volumes/Fotos-B", totalBytes: 2e12, availableBytes: 1.21e12, removable: true },
];

/** Source and target as the window remembers them. */
export const REMEMBERED = {
  source: { path: "/Volumes/Fotos-A", name: "Fotos-A", kind: "volume", free: 412e9, total: 2e12 },
  target: { path: "/Volumes/Fotos-B", name: "Fotos-B", kind: "volume", free: 1.21e12, total: 2e12 },
};

function install(backend: Backend): void {
  const callbacks = new Map<number, (data: unknown) => void>();
  const listeners: Record<string, number[]> = {};
  let next = 1;
  window.__backend = backend;
  window.__calls = [];

  /** Finds width and height in a serialized size, whatever its wrapping. */
  const size = (v: unknown): [number, number] | null => {
    if (!v || typeof v !== "object") return null;
    const o = v as Record<string, unknown>;
    if (typeof o.width === "number" && typeof o.height === "number") return [o.width, o.height];
    for (const inner of Object.values(o)) {
      const s = size(inner);
      if (s) return s;
    }
    return null;
  };

  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main", windowLabel: "main" } },
    plugins: { path: { sep: "/", delimiter: ":" } },
    convertFileSrc: (p: string) => p,
    transformCallback(cb: (data: unknown) => void) {
      const id = next++;
      callbacks.set(id, cb);
      return id;
    },
    unregisterCallback(id: number) {
      callbacks.delete(id);
    },
    async invoke(cmd: string, args: Record<string, unknown>) {
      window.__calls.push({ cmd, args });
      const b = window.__backend;
      switch (cmd) {
        case "plugin:event|listen":
          (listeners[args.event as string] ??= []).push(args.handler as number);
          return args.handler;
        case "plugin:window|set_size": {
          const s = size(args.value);
          if (s) await window.__resize(s[0], s[1]);
          return null;
        }
        case "locales":
          return b.locales;
        case "list_volumes":
          return b.volumes ?? [];
        case "check_pair":
          return b.checkPair ?? null;
        case "start":
          if (b.startError) throw b.startError;
          return null;
        case "plugin:notification|is_permission_granted":
          return true;
        default:
          return null;
      }
    },
  };

  window.__emit = (event, payload) => {
    for (const id of listeners[event] ?? []) callbacks.get(id)?.({ event, id, payload });
  };
}

/**
 * Opens the window with the given backend and remembered choices. JavaScript
 * errors are collected in `errors`.
 */
export async function open(
  page: Page,
  backend: Backend,
  storage: Record<string, unknown> = {},
  errors: string[] = [],
): Promise<void> {
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  await page.exposeFunction("__resize", (width: number, height: number) =>
    page.setViewportSize({ width, height }),
  );
  await page.addInitScript(install, { volumes: DISKS, ...backend });
  await page.addInitScript((s) => {
    localStorage.clear();
    for (const [k, v] of Object.entries(s)) localStorage.setItem(k, typeof v === "string" ? v : JSON.stringify(v));
  }, storage);
  await page.goto("/");
  // The window lists the disks right after it has set up its language and
  // controls.
  await page.waitForFunction(() => window.__calls.some((c) => c.cmd === "list_volumes"));
}

/** Sends a backend event to the window. */
export async function emit(page: Page, event: string, payload: unknown): Promise<void> {
  await page.evaluate(([e, p]) => window.__emit(e as string, p), [event, payload] as const);
}
