// The scan view: every planned change as a tree or a flat list, with
// category filters and a search. Only the visible rows are drawn, so a first
// scan onto an empty disk (hundreds of thousands of entries) stays smooth.

import * as fmt from "./format";

/** One planned change, as sent by the backend (compact field names). */
export interface ScanItem {
  /** c = copy, m = new folder, d = delete, x = replace, r = rename */
  k: "c" | "m" | "d" | "x" | "r";
  /** Relative path. */
  p: string;
  /** Size (copy) or number of entries (delete). */
  n: number;
  dir?: boolean;
  /** Current name on the target (rename). */
  from?: string;
}

type Category = "copy" | "delete" | "rename";

const CATEGORY: Record<ScanItem["k"], Category> = {
  c: "copy",
  m: "copy",
  d: "delete",
  x: "delete",
  r: "rename",
};

const ROW_HEIGHT = 24;
/** Rows drawn above and below the visible area. */
const OVERSCAN = 12;

interface Counts {
  copy: number;
  delete: number;
  rename: number;
}

interface Node {
  name: string;
  path: string;
  depth: number;
  children: Map<string, Node>;
  /** Changes at exactly this path. */
  items: ScanItem[];
  /** Changes at or below this path. */
  counts: Counts;
}

interface Row {
  depth: number;
  /** Folder that can be opened (has children). */
  folder?: Node;
  open?: boolean;
  label: string;
  /** Sign and colour of the change shown in front of the label. */
  sign?: Category;
  /** Trusted HTML: built only from numbers and escaped names. */
  detail: string;
  title: string;
}

export class ScanView {
  private items: ScanItem[] = [];
  private shown = new Set<Category>(["copy", "delete", "rename"]);
  private query = "";
  private mode: "tree" | "list" = "tree";
  private open = new Set<string>();
  private rows: Row[] = [];
  private searchTimer = 0;

  constructor(
    private readonly scroller: HTMLElement,
    private readonly spacer: HTMLElement,
    private readonly status: HTMLElement,
  ) {
    scroller.addEventListener("scroll", () => this.draw());
    new ResizeObserver(() => this.draw()).observe(scroller);
    scroller.addEventListener("click", (e) => {
      const el = (e.target as HTMLElement).closest<HTMLElement>(".row[data-folder]");
      if (!el) return;
      const path = el.dataset.folder!;
      if (this.open.has(path)) this.open.delete(path);
      else this.open.add(path);
      this.rebuild(false);
    });
  }

  /** Shows a new scan result. */
  load(items: ScanItem[]): void {
    this.items = items;
    this.open.clear();
    // Start with the top level open: the first look shows where changes are.
    this.scroller.scrollTop = 0;
    this.rebuild(true);
  }

  setCategory(cat: Category, on: boolean): void {
    if (on) this.shown.add(cat);
    else this.shown.delete(cat);
    this.rebuild(false);
  }

  setSearch(text: string): void {
    window.clearTimeout(this.searchTimer);
    this.searchTimer = window.setTimeout(() => {
      this.query = text.trim().toLowerCase();
      this.scroller.scrollTop = 0;
      this.rebuild(false);
    }, 120);
  }

  setMode(mode: "tree" | "list"): void {
    this.mode = mode;
    this.scroller.scrollTop = 0;
    this.rebuild(false);
  }

  private visibleItems(): ScanItem[] {
    const q = this.query;
    return this.items.filter(
      (it) =>
        this.shown.has(CATEGORY[it.k]) &&
        (q === "" || it.p.toLowerCase().includes(q) || (it.from?.toLowerCase().includes(q) ?? false)),
    );
  }

  private rebuild(openTopLevel: boolean): void {
    const items = this.visibleItems();
    if (this.mode === "list") {
      this.rows = items.map((it) => itemRow(it, it.p, 0));
    } else {
      const root = buildTree(items);
      if (openTopLevel) {
        for (const child of root.children.values()) this.open.add(child.path);
      }
      // While searching, every match is shown with its folders opened.
      const openAll = this.query !== "";
      this.rows = [];
      flatten(root, this.rows, (n) => openAll || this.open.has(n.path));
    }
    this.status.textContent =
      items.length === this.items.length
        ? fmt.plural(items.length, "change")
        : `${fmt.count(items.length)} of ${fmt.plural(this.items.length, "change")}`;
    this.spacer.style.height = `${this.rows.length * ROW_HEIGHT}px`;
    this.draw();
  }

  private draw(): void {
    const top = this.scroller.scrollTop;
    const first = Math.max(0, Math.floor(top / ROW_HEIGHT) - OVERSCAN);
    const last = Math.min(
      this.rows.length,
      Math.ceil((top + this.scroller.clientHeight) / ROW_HEIGHT) + OVERSCAN,
    );
    const html: string[] = [];
    for (let i = first; i < last; i++) html.push(renderRow(this.rows[i], i));
    if (this.rows.length === 0) {
      html.push(`<div class="rows-empty">${this.items.length === 0 ? "No changes." : "Nothing matches."}</div>`);
    }
    // The spacer gives the scrollbar its full height; rows sit on top of it.
    this.spacer.innerHTML = html.join("");
  }
}

function emptyCounts(): Counts {
  return { copy: 0, delete: 0, rename: 0 };
}

function weight(it: ScanItem): Counts {
  const c = emptyCounts();
  if (it.k === "c") c.copy = 1;
  else if (it.k === "d") c.delete = it.n;
  else if (it.k === "x") c.delete = 1;
  else if (it.k === "r") c.rename = 1;
  return c;
}

function buildTree(items: ScanItem[]): Node {
  const root: Node = { name: "", path: "", depth: -1, children: new Map(), items: [], counts: emptyCounts() };
  for (const it of items) {
    const w = weight(it);
    let node = root;
    add(node.counts, w);
    const parts = it.p.split("/");
    let path = "";
    for (const part of parts) {
      path = path ? `${path}/${part}` : part;
      let child = node.children.get(part);
      if (!child) {
        child = { name: part, path, depth: node.depth + 1, children: new Map(), items: [], counts: emptyCounts() };
        node.children.set(part, child);
      }
      add(child.counts, w);
      node = child;
    }
    node.items.push(it);
  }
  return root;
}

function add(into: Counts, w: Counts): void {
  into.copy += w.copy;
  into.delete += w.delete;
  into.rename += w.rename;
}

/** Folders first, then by name — like Finder. */
function sorted(node: Node): Node[] {
  return [...node.children.values()].sort((a, b) => {
    const fa = a.children.size > 0 ? 0 : 1;
    const fb = b.children.size > 0 ? 0 : 1;
    return fa - fb || a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" });
  });
}

function flatten(node: Node, out: Row[], isOpen: (n: Node) => boolean): void {
  for (const child of sorted(node)) {
    if (child.children.size === 0) {
      // A leaf: the change(s) at this path.
      for (const it of child.items) out.push(itemRow(it, child.name, child.depth));
      continue;
    }
    const open = isOpen(child);
    const tags = child.items.map(folderTag).filter(Boolean).join(", ");
    out.push({
      depth: child.depth,
      folder: child,
      open,
      label: child.name,
      sign: child.items.some((i) => i.k === "m") ? "copy" : undefined,
      detail: [tags, countsText(child.counts)].filter(Boolean).join(" · "),
      title: child.path,
    });
    if (open) flatten(child, out, isOpen);
  }
}

function folderTag(it: ScanItem): string {
  if (it.k === "m") return "new folder";
  if (it.k === "x") return "replaces a file";
  if (it.k === "r") return `renamed from ${escapeHtml(it.from ?? "")}`;
  return "";
}

function countsText(c: Counts): string {
  const parts: string[] = [];
  if (c.copy) parts.push(`<span class="sign copy">＋${fmt.count(c.copy)}</span>`);
  if (c.delete) parts.push(`<span class="sign delete">−${fmt.count(c.delete)}</span>`);
  if (c.rename) parts.push(`<span class="sign rename">↻${fmt.count(c.rename)}</span>`);
  return parts.join(" ");
}

function itemRow(it: ScanItem, label: string, depth: number): Row {
  switch (it.k) {
    case "c":
      return { depth, label, sign: "copy", detail: fmt.bytes(it.n), title: it.p };
    case "m":
      return { depth, label: `${label}/`, sign: "copy", detail: "new folder", title: it.p };
    case "d":
      return {
        depth,
        label: it.dir ? `${label}/` : label,
        sign: "delete",
        detail: it.dir ? `whole folder, ${fmt.plural(it.n, "entry", "entries")}` : "",
        title: it.p,
      };
    case "x":
      return { depth, label, sign: "delete", detail: "replaced (other type on the target)", title: it.p };
    case "r":
      return { depth, label, sign: "rename", detail: `renamed from ${escapeHtml(it.from ?? "")}`, title: it.p };
  }
}

const SIGN: Record<Category, string> = { copy: "＋", delete: "−", rename: "↻" };

function renderRow(r: Row, index: number): string {
  const indent = 10 + r.depth * 16;
  const disclosure = r.folder ? `<span class="disclosure">${r.open ? "▾" : "▸"}</span>` : `<span class="disclosure"></span>`;
  const sign = r.sign ? `<span class="sign ${r.sign}">${SIGN[r.sign]}</span>` : "";
  const icon = r.folder ? `<span class="folder-icon"></span>` : "";
  const attrs = r.folder ? ` data-folder="${escapeAttr(r.folder.path)}"` : "";
  return (
    `<div class="row${r.folder ? " folder" : ""} ${r.sign ?? ""}"${attrs} style="top:${index * ROW_HEIGHT}px;padding-left:${indent}px" title="${escapeAttr(r.title)}">` +
    `${disclosure}${sign}${icon}<span class="row-label">${escapeHtml(r.label)}</span>` +
    `<span class="row-detail">${r.detail}</span></div>`
  );
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]!);
}

function escapeAttr(s: string): string {
  return escapeHtml(s).replace(/"/g, "&quot;");
}
