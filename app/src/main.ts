// dino-copy desktop app: pick source and target, press Start, watch it run.

import { invoke } from "@tauri-apps/api/core";
import { LogicalPosition, LogicalSize } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import { Menu } from "@tauri-apps/api/menu";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { isPermissionGranted, requestPermission } from "@tauri-apps/plugin-notification";
import * as fmt from "./format";
import { ScanView, type ScanItem } from "./scan";

type Mode = "mirror" | "scan";
type Side = "source" | "target";
type Phase = "scanning" | "copying" | "deleting";
type MenuItems = NonNullable<NonNullable<Parameters<typeof Menu.new>[0]>["items"]>;

interface Volume {
  name: string;
  path: string;
  totalBytes: number;
  availableBytes: number;
  removable: boolean;
}

/** What a picker currently shows. */
interface Choice {
  path: string;
  name: string;
  kind: "volume" | "folder";
  /** Free space, as seen when the disk was picked. */
  detail?: string;
}

interface Progress {
  bytesDone: number;
  bytesTotal: number;
  filesDone: number;
  filesTotal: number;
  current: string;
  bytesPerSec: number;
  etaSecs: number | null;
}

interface Summary {
  copyCount: number;
  copyBytes: number;
  deleteCount: number;
  renameCount: number;
  mkdirCount: number;
  unchangedCount: number;
}

interface RunResult {
  mode: Mode;
  outcome: "success" | "inSync" | "scanned" | "incomplete" | "failed" | "cancelled";
  headline: string;
  messages: string[];
  summary: Summary;
  durationSecs: number;
  items?: ScanItem[];
}

const SMALL = new LogicalSize(520, 300);
const LARGE = new LogicalSize(880, 660);

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const win = getCurrentWindow();

const state = {
  source: load<Choice>("source"),
  target: load<Choice>("target"),
  mode: (localStorage.getItem("mode") as Mode | null) ?? "mirror",
  running: false,
  closeWhenDone: false,
  /** When copying began: the time estimate and the bar measure from here. */
  copyStartedAt: 0,
  lastResult: null as RunResult | null,
};

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

function show(view: "setup" | "running" | "done" | "scan"): void {
  for (const id of ["setup", "running", "done", "scan"]) $(id).hidden = id !== view;
}

async function useLargeWindow(large: boolean): Promise<void> {
  await win.setResizable(large);
  await win.setMinSize(large ? new LogicalSize(680, 460) : null);
  await win.setSize(large ? LARGE : SMALL);
  await win.center();
}

// ---------------------------------------------------------------------------
// 1 · Setup: pickers and the Start split button
// ---------------------------------------------------------------------------

function renderPicker(side: Side): void {
  const choice = state[side];
  const el = $(`pick-${side}`);
  el.querySelector(".tile-name")!.textContent = choice?.name ?? "Choose…";
  const detail = el.querySelector(".tile-detail")!;
  detail.classList.toggle("path", choice?.kind === "folder");
  if (choice?.kind === "folder") setPath(detail, choice.path);
  else detail.textContent = choice ? (choice.detail ?? choice.path) : "a disk or a folder";
  el.querySelector(".disk-icon")!.className = `disk-icon ${choice?.kind ?? "none"}`;
  el.classList.toggle("empty", !choice);
}

async function openPicker(side: Side): Promise<void> {
  const el = $(`pick-${side}`);
  el.classList.add("active");
  // Read fresh every time: a disk plugged in a moment ago must show up.
  const volumes = await invoke<Volume[]>("list_volumes");
  const current = state[side];
  const items: MenuItems = [];
  for (const v of volumes) {
    const used = v.totalBytes - v.availableBytes;
    const free = `${fmt.bytes(v.availableBytes)} free of ${fmt.bytes(v.totalBytes)}`;
    items.push({
      text: `${v.name}  ·  ${fmt.bytes(used)} used`,
      checked: current?.path === v.path,
      action: () => choose(side, { path: v.path, name: v.name, kind: "volume", detail: free }),
    });
  }
  if (volumes.length === 0) items.push({ text: "No external disks found", enabled: false });
  if (current?.kind === "folder") {
    items.push({ item: "Separator" });
    items.push({ text: current.path, checked: true, action: () => {} });
  }
  items.push({ item: "Separator" });
  items.push({ text: "Choose Folder…", action: () => void chooseFolder(side) });
  const menu = await Menu.new({ items });
  const rect = el.getBoundingClientRect();
  await menu.popup(new LogicalPosition(rect.left + 8, rect.bottom - 8));
  el.classList.remove("active");
}

async function chooseFolder(side: Side): Promise<void> {
  const path = await open({ directory: true, multiple: false, title: `Choose the ${side} folder` });
  if (typeof path !== "string") return;
  choose(side, { path, name: path.split("/").filter(Boolean).pop() ?? path, kind: "folder" });
}

function choose(side: Side, choice: Choice): void {
  state[side] = choice;
  localStorage.setItem(side, JSON.stringify(choice));
  renderPicker(side);
  void validate();
}

/** Enables Start only for a pair the backend accepts, and says why not. */
async function validate(): Promise<void> {
  const hint = $("setup-hint");
  if (!state.source || !state.target) {
    hint.textContent = "Choose a source and a target.";
    setStartEnabled(false);
    return;
  }
  const problem = await invoke<string | null>("check_pair", {
    source: state.source.path,
    target: state.target.path,
  });
  hint.textContent = problem ?? (state.mode === "mirror" ? "The target will become an exact copy of the source." : "Shows what a mirror would do. Nothing is changed.");
  hint.classList.toggle("problem", problem !== null);
  setStartEnabled(problem === null);
}

/**
 * A disabled Start turns the whole split button neutral (as macOS does with a
 * disabled default button); the arrow stays usable to switch the mode.
 */
function setStartEnabled(on: boolean): void {
  $<HTMLButtonElement>("start").disabled = !on;
  document.querySelector(".split-button")!.classList.toggle("off", !on);
}

function renderMode(): void {
  $("start").textContent = state.mode === "mirror" ? "Mirror" : "Scan";
}

async function openModeMenu(): Promise<void> {
  const setMode = (mode: Mode) => {
    state.mode = mode;
    localStorage.setItem("mode", mode);
    renderMode();
    void validate();
  };
  const menu = await Menu.new({
    items: [
      { text: "Mirror", checked: state.mode === "mirror", action: () => setMode("mirror") },
      { text: "Scan (only show what would happen)", checked: state.mode === "scan", action: () => setMode("scan") },
    ],
  });
  const rect = $("start-menu").getBoundingClientRect();
  await menu.popup(new LogicalPosition(rect.right - 200, rect.bottom + 4));
}

// ---------------------------------------------------------------------------
// 2 · Running
// ---------------------------------------------------------------------------

async function start(mode: Mode): Promise<void> {
  if (!state.source || !state.target) return;
  if (mode === "mirror" && !(await isPermissionGranted())) {
    // Asked once; the run does not depend on the answer.
    void requestPermission();
  }
  setCaption($("run-caption"), mode === "mirror" ? "Mirroring" : "Scanning");
  onPhase("scanning");
  for (const id of ["run-bytes", "run-rate", "run-files"]) $(id).textContent = "–";
  $("run-current").textContent = "";
  try {
    await invoke("start", { mode, source: state.source.path, target: state.target.path });
  } catch (e) {
    $("setup-hint").textContent = String(e);
    $("setup-hint").classList.add("problem");
    return;
  }
  state.running = true;
  if (!$("scan").hidden) await useLargeWindow(false);
  show("running");
}

function onPhase(phase: Phase): void {
  const cancel = $<HTMLButtonElement>("cancel");
  const bar = $("run-bar");
  if (phase === "scanning") {
    $("run-eta").textContent = "Comparing…";
    $("run-phase").textContent = "Looking for what changed since the last mirror";
    bar.classList.add("indeterminate");
    cancel.disabled = false;
    cancel.textContent = "Cancel";
  } else if (phase === "copying") {
    state.copyStartedAt = performance.now();
    $("run-eta").textContent = "Estimating time left…";
    $("run-phase").textContent = "Copying new and changed files";
    bar.classList.remove("indeterminate");
    bar.style.width = "0%";
  } else {
    $("run-eta").textContent = "Almost done";
    $("run-phase").textContent = "Removing what is no longer in the source";
    bar.classList.add("indeterminate");
    // Half-done removals would be worse than finished ones.
    cancel.disabled = true;
    cancel.textContent = "Finishing…";
  }
}

function onScanProgress(entries: number): void {
  $("run-files").textContent = `${fmt.count(entries)} compared`;
}

function onProgress(p: Progress): void {
  const elapsed = (performance.now() - state.copyStartedAt) / 1000;
  // Progress by time, consistent with the estimate (bytes alone mislead when
  // many tiny files remain).
  const fraction =
    p.etaSecs !== null && elapsed + p.etaSecs > 0
      ? elapsed / (elapsed + p.etaSecs)
      : p.bytesTotal > 0
        ? p.bytesDone / p.bytesTotal
        : p.filesDone / Math.max(1, p.filesTotal);
  $("run-bar").style.width = `${Math.min(100, fraction * 100).toFixed(1)}%`;
  $("run-eta").textContent = p.etaSecs === null ? "Estimating time left…" : fmt.timeLeft(p.etaSecs);
  $("run-bytes").textContent = `${fmt.bytes(p.bytesDone)} of ${fmt.bytes(p.bytesTotal)}`;
  $("run-rate").textContent = p.bytesPerSec > 0 ? `${fmt.bytes(p.bytesPerSec)}/s` : "–";
  $("run-files").textContent = `${fmt.count(p.filesDone)} of ${fmt.count(p.filesTotal)}`;
  setPath($("run-current"), p.current);
}

// ---------------------------------------------------------------------------
// 3 · Done, 4 · Scan
// ---------------------------------------------------------------------------

async function onDone(r: RunResult): Promise<void> {
  state.running = false;
  state.lastResult = r;
  if (state.closeWhenDone) {
    await win.destroy();
    return;
  }
  if (r.outcome === "scanned" && r.items) {
    await showScan(r);
    return;
  }
  $("done-icon").className = `done-icon ${r.outcome}`;
  $("done-headline").textContent = r.headline;
  $("done-summary").textContent = summaryText(r);
  $("done-messages").innerHTML = "";
  for (const m of r.messages) {
    const li = document.createElement("li");
    li.textContent = m;
    $("done-messages").append(li);
  }
  show("done");
}

function summaryText(r: RunResult): string {
  const s = r.summary;
  if (r.outcome === "inSync" || r.outcome === "failed") return `Took ${fmt.duration(r.durationSecs)}.`;
  const parts = [`${fmt.plural(s.copyCount, "file")} copied (${fmt.bytes(s.copyBytes)})`];
  if (s.deleteCount) parts.push(`${fmt.count(s.deleteCount)} deleted`);
  if (s.renameCount) parts.push(`${fmt.count(s.renameCount)} renamed`);
  parts.push(fmt.duration(r.durationSecs));
  return r.outcome === "cancelled" ? `Cancelled after ${fmt.duration(r.durationSecs)}.` : parts.join(" · ");
}

const scanView = new ScanView($("scan-rows"), $("scan-spacer"), $("scan-shown"));

async function showScan(r: RunResult): Promise<void> {
  const s = r.summary;
  setCaption($("scan-caption"), "Scan");
  $("scan-summary").textContent =
    s.copyCount + s.deleteCount + s.renameCount === 0
      ? "Nothing to do: the target already mirrors the source."
      : `A mirror would copy ${fmt.plural(s.copyCount, "file")} (${fmt.bytes(s.copyBytes)}), delete ${fmt.count(s.deleteCount)} and rename ${fmt.count(s.renameCount)}.`;
  $("scan-note").textContent = [
    `${fmt.plural(s.unchangedCount, "file")} unchanged.`,
    ...r.messages,
  ].join(" ");
  $("count-copy").textContent = fmt.count(s.copyCount);
  $("count-delete").textContent = fmt.count(s.deleteCount);
  $("count-rename").textContent = fmt.count(s.renameCount);
  show("scan");
  await useLargeWindow(true);
  scanView.load(r.items ?? []);
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/** "MIRRORING  Fotos-A → Fotos-B": a small label, then the route in plain text. */
function setCaption(el: HTMLElement, label: string): void {
  const route = document.createElement("span");
  route.className = "caption-route";
  route.textContent = `${state.source?.name ?? ""} → ${state.target?.name ?? ""}`;
  el.replaceChildren(label, route);
}

/**
 * Shows a path that, when too long, is cut at the start ("…/2024/IMG_1.CR3").
 * The element is laid out right-to-left for that; the <bdi> keeps the path
 * itself left-to-right, so a leading "/" stays in front.
 */
function setPath(el: Element, path: string): void {
  const bdi = document.createElement("bdi");
  bdi.textContent = path;
  el.replaceChildren(bdi);
}

function load<T>(key: string): T | null {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

function wire(): void {
  $("pick-source").addEventListener("click", () => void openPicker("source"));
  $("pick-target").addEventListener("click", () => void openPicker("target"));
  $("start").addEventListener("click", () => void start(state.mode));
  $("start-menu").addEventListener("click", () => void openModeMenu());
  $("cancel").addEventListener("click", () => {
    $<HTMLButtonElement>("cancel").disabled = true;
    $("run-phase").textContent = "Cancelling…";
    void invoke("cancel");
  });
  $("done-ok").addEventListener("click", () => {
    show("setup");
    void validate();
  });
  $("scan-close").addEventListener("click", async () => {
    show("setup");
    await useLargeWindow(false);
    void validate();
  });
  $("scan-mirror").addEventListener("click", () => void start("mirror"));
  $("scan-search").addEventListener("input", (e) => scanView.setSearch((e.target as HTMLInputElement).value));
  for (const chip of document.querySelectorAll<HTMLButtonElement>(".chip")) {
    chip.addEventListener("click", () => {
      chip.classList.toggle("on");
      scanView.setCategory(chip.dataset.cat as "copy" | "delete" | "rename", chip.classList.contains("on"));
    });
  }
  for (const [id, mode] of [["view-tree", "tree"], ["view-list", "list"]] as const) {
    $(id).addEventListener("click", () => {
      $("view-tree").classList.toggle("on", mode === "tree");
      $("view-list").classList.toggle("on", mode === "list");
      scanView.setMode(mode);
    });
  }

  void listen<Phase>("run://phase", (e) => onPhase(e.payload));
  void listen<number>("run://scan-progress", (e) => onScanProgress(e.payload));
  void listen<Progress>("run://progress", (e) => onProgress(e.payload));
  void listen<RunResult>("run://done", (e) => void onDone(e.payload));

  // Closing the window mid-run cancels first, so that no temp files are left
  // behind, and closes once the run has stopped.
  void win.onCloseRequested(async (e) => {
    if (!state.running) return;
    e.preventDefault();
    state.closeWhenDone = true;
    await invoke("cancel");
  });
}

wire();
renderPicker("source");
renderPicker("target");
renderMode();
void validate();
