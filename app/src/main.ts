// dino-copy desktop app: pick source and target, press Start, watch it run.

import { invoke } from "@tauri-apps/api/core";
import { LogicalPosition, LogicalSize } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import { Menu } from "@tauri-apps/api/menu";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import * as fmt from "./format";
import { setLanguage, t, translatePage } from "./i18n";
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
  /** Free and total bytes, as seen when the disk was picked. */
  free?: number;
  total?: number;
}

/** "120 GB free of 2 TB" for a disk (in the current language). */
function volumeDetail(c: Choice): string {
  if (c.free === undefined || c.total === undefined) return c.path;
  return t("freeOf", { free: fmt.bytes(c.free), total: fmt.bytes(c.total) });
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

/** A message from the backend, turned into a sentence by {@link msgText}. */
type Msg =
  | { key: "sourceMissing" | "targetMissing" | "unresolvable"; path: string }
  | {
      key:
        | "identical"
        | "targetInsideSource"
        | "sourceInsideTarget"
        | "runInProgress"
        | "cancelledUnchanged"
        | "scanNothingToDo"
        | "scanFinished"
        | "inSync"
        | "cancelledPartly"
        | "finishedIncomplete";
    }
  | { key: "sourceEmpty"; entries: number }
  | { key: "finishedWithErrors" | "skippedAfterFailures" | "skippedUnreadable" | "skippedChanged"; count: number }
  | { key: "mirrored"; target: string; source: string }
  | { key: "deletionsWillBeSkipped"; count: number; unreadable: number; collisions: number }
  | { key: "rootMetaFailed"; error: string }
  | { key: "detail"; text: string };

interface RunResult {
  mode: Mode;
  outcome: "success" | "inSync" | "scanned" | "incomplete" | "failed" | "cancelled";
  headline: Msg;
  messages: Msg[];
  summary: Summary;
  durationSecs: number;
  items?: ScanItem[];
}

const SMALL = new LogicalSize(520, 320);
const TALL = new LogicalSize(520, 480);
const LARGE = new LogicalSize(880, 660);

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const win = getCurrentWindow();

const state = {
  source: load<Choice>("source"),
  target: load<Choice>("target"),
  // Anything but a known mode (an older or damaged setting) means Mirror.
  mode: (localStorage.getItem("mode") === "scan" ? "scan" : "mirror") as Mode,
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

/**
 * small: setup, progress and a plain result; tall: a result with messages,
 * so that they can be read; large (resizable): the scan list.
 */
let windowSize: "small" | "tall" | "large" = "small";

async function setWindow(size: "small" | "tall" | "large"): Promise<void> {
  // Unchanged size: leave the window where the user put it.
  if (size === windowSize) return;
  windowSize = size;
  const large = size === "large";
  await win.setResizable(large);
  await win.setMinSize(large ? new LogicalSize(680, 460) : null);
  await win.setSize(size === "small" ? SMALL : size === "tall" ? TALL : LARGE);
  await win.center();
}

// ---------------------------------------------------------------------------
// 1 · Setup: pickers and the Start split button
// ---------------------------------------------------------------------------

function renderPicker(side: Side): void {
  const choice = state[side];
  const el = $(`pick-${side}`);
  el.querySelector(".tile-name")!.textContent = choice?.name ?? t("choose");
  const detail = el.querySelector(".tile-detail")!;
  detail.classList.toggle("path", choice?.kind === "folder");
  if (choice?.kind === "folder") setPath(detail, choice.path);
  else detail.textContent = choice ? volumeDetail(choice) : t("diskOrFolder");
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
    items.push({
      text: `${v.name}  ·  ${t("used", { used: fmt.bytes(used) })}`,
      checked: current?.path === v.path,
      action: () =>
        choose(side, { path: v.path, name: v.name, kind: "volume", free: v.availableBytes, total: v.totalBytes }),
    });
  }
  if (volumes.length === 0) items.push({ text: t("noDisks"), enabled: false });
  if (current?.kind === "folder") {
    items.push({ item: "Separator" });
    items.push({ text: current.path, checked: true, action: () => {} });
  }
  items.push({ item: "Separator" });
  items.push({ text: t("chooseFolder"), action: () => void chooseFolder(side) });
  const menu = await Menu.new({ items });
  const rect = el.getBoundingClientRect();
  await menu.popup(new LogicalPosition(rect.left + 8, rect.bottom - 8));
  el.classList.remove("active");
}

async function chooseFolder(side: Side): Promise<void> {
  const path = await open({
    directory: true,
    multiple: false,
    title: t(side === "source" ? "chooseSourceFolder" : "chooseTargetFolder"),
  });
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
    hint.textContent = t("hintChoose");
    setStartEnabled(false);
    return;
  }
  const problem = await invoke<Msg | null>("check_pair", {
    source: state.source.path,
    target: state.target.path,
  });
  hint.textContent = problem ? msgText(problem) : t(state.mode === "mirror" ? "hintMirror" : "hintScan");
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
  $("start").textContent = t(state.mode);
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
      { text: t("mirror"), checked: state.mode === "mirror", action: () => setMode("mirror") },
      { text: t("scan"), checked: state.mode === "scan", action: () => setMode("scan") },
    ],
  });
  const rect = $("start-menu").getBoundingClientRect();
  await menu.popup(new LogicalPosition(rect.left, rect.bottom + 4));
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
  setCaption($("run-caption"), t(mode === "mirror" ? "mirroring" : "scanning"));
  onPhase("scanning");
  for (const id of ["run-bytes", "run-rate", "run-files"]) $(id).textContent = "–";
  $("run-current").textContent = "";
  try {
    await invoke("start", { mode, source: state.source.path, target: state.target.path });
  } catch (e) {
    $("setup-hint").textContent = typeof e === "object" && e !== null && "key" in e ? msgText(e as Msg) : String(e);
    $("setup-hint").classList.add("problem");
    return;
  }
  state.running = true;
  await setWindow("small");
  show("running");
}

function onPhase(phase: Phase): void {
  const cancel = $<HTMLButtonElement>("cancel");
  const bar = $("run-bar");
  if (phase === "scanning") {
    $("run-eta").textContent = t("comparing");
    $("run-phase").textContent = t("phaseScanning");
    bar.classList.add("indeterminate");
    cancel.disabled = false;
    cancel.textContent = t("cancel");
  } else if (phase === "copying") {
    state.copyStartedAt = performance.now();
    $("run-eta").textContent = t("estimating");
    $("run-phase").textContent = t("phaseCopying");
    bar.classList.remove("indeterminate");
    bar.style.width = "0%";
  } else {
    $("run-eta").textContent = t("almostDone");
    $("run-phase").textContent = t("phaseDeleting");
    bar.classList.add("indeterminate");
    // Half-done removals would be worse than finished ones.
    cancel.disabled = true;
    cancel.textContent = t("finishing");
  }
}

function onScanProgress(entries: number): void {
  $("run-files").textContent = t("compared", { n: entries });
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
  $("run-eta").textContent = p.etaSecs === null ? t("estimating") : fmt.timeLeft(p.etaSecs);
  $("run-bytes").textContent = t("xOfY", { done: fmt.bytes(p.bytesDone), total: fmt.bytes(p.bytesTotal) });
  $("run-rate").textContent = p.bytesPerSec > 0 ? `${fmt.bytes(p.bytesPerSec)}/s` : "–";
  $("run-files").textContent = t("xOfY", { done: p.filesDone, total: p.filesTotal });
  setPath($("run-current"), p.current);
}

// ---------------------------------------------------------------------------
// 3 · Done, 4 · Scan
// ---------------------------------------------------------------------------

async function onDone(r: RunResult): Promise<void> {
  state.running = false;
  state.lastResult = r;
  if (r.mode === "mirror") void notify(msgText(r.headline));
  if (state.closeWhenDone) {
    await win.destroy();
    return;
  }
  if (r.outcome === "scanned" && r.items) {
    await showScan(r);
    return;
  }
  $("done-icon").className = `done-icon ${r.outcome}`;
  $("done-headline").textContent = msgText(r.headline);
  $("done-summary").textContent = summaryText(r);
  $("done-messages").innerHTML = "";
  for (const m of r.messages) {
    const li = document.createElement("li");
    li.textContent = msgText(m);
    $("done-messages").append(li);
  }
  show("done");
  if (r.messages.length > 0) await setWindow("tall");
}

function summaryText(r: RunResult): string {
  const s = r.summary;
  const took = fmt.duration(r.durationSecs);
  if (r.outcome === "inSync" || r.outcome === "failed") return t("took", { duration: took });
  if (r.outcome === "cancelled") return t("cancelledAfter", { duration: took });
  const parts = [t("filesCopied", { n: s.copyCount, bytes: fmt.bytes(s.copyBytes) })];
  if (s.deleteCount) parts.push(t("deleted", { n: s.deleteCount }));
  if (s.renameCount) parts.push(t("renamed", { n: s.renameCount }));
  parts.push(took);
  return parts.join(" · ");
}

const scanView = new ScanView($("scan-rows"), $("scan-spacer"), $("scan-shown"));

async function showScan(r: RunResult): Promise<void> {
  const s = r.summary;
  setCaption($("scan-caption"), t("scanning"));
  $("scan-summary").textContent =
    s.copyCount + s.deleteCount + s.renameCount === 0
      ? t("scanNothingToDo")
      : t("scanWould", {
          files: t("fileCount", { n: s.copyCount }),
          bytes: fmt.bytes(s.copyBytes),
          deleted: s.deleteCount,
          renamed: s.renameCount,
        });
  $("scan-note").textContent = [t("unchanged", { n: s.unchangedCount }), ...r.messages.map(msgText)].join(" ");
  $("count-copy").textContent = fmt.count(s.copyCount);
  $("count-delete").textContent = fmt.count(s.deleteCount);
  $("count-rename").textContent = fmt.count(s.renameCount);
  // Nothing to mirror: offer only Close.
  $("scan-mirror").hidden = s.copyCount + s.deleteCount + s.renameCount === 0;
  show("scan");
  await setWindow("large");
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

/** A backend message as a sentence in the current language. */
function msgText(m: Msg): string {
  switch (m.key) {
    case "sourceMissing":
    case "targetMissing":
    case "unresolvable":
      return t(m.key, { path: m.path });
    case "sourceEmpty":
      return t(m.key, { n: m.entries });
    case "finishedWithErrors":
    case "skippedAfterFailures":
    case "skippedUnreadable":
    case "skippedChanged":
      return t(m.key, { n: m.count });
    case "mirrored":
      return t(m.key, { target: m.target, source: m.source });
    case "deletionsWillBeSkipped": {
      const reasons = [];
      if (m.unreadable) reasons.push(t("reasonUnreadable", { n: m.unreadable }));
      if (m.collisions) reasons.push(t("reasonCollisions", { n: m.collisions }));
      return t(m.key, { n: m.count, reason: reasons.join("; ") });
    }
    case "rootMetaFailed":
      return t(m.key, { error: m.error });
    case "detail":
      return m.text;
    default:
      return t(m.key);
  }
}

/** Tells the user a mirror has ended, in case the window is in the background. */
async function notify(body: string): Promise<void> {
  if (await isPermissionGranted()) sendNotification({ title: "dino-copy", body });
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
    $("run-phase").textContent = t("cancelling");
    void invoke("cancel");
  });
  $("done-ok").addEventListener("click", async () => {
    show("setup");
    await setWindow("small");
    void validate();
  });
  $("scan-close").addEventListener("click", async () => {
    show("setup");
    await setWindow("small");
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

/** Free space changes between runs: refresh it for remembered disks. */
async function refreshVolumes(): Promise<void> {
  const volumes = await invoke<Volume[]>("list_volumes").catch(() => []);
  for (const side of ["source", "target"] as const) {
    const choice = state[side];
    const v = volumes.find((v) => v.path === choice?.path);
    if (choice?.kind !== "volume" || !v) continue;
    state[side] = { ...choice, free: v.availableBytes, total: v.totalBytes };
    localStorage.setItem(side, JSON.stringify(state[side]));
    renderPicker(side);
  }
}

setLanguage(await invoke<string[]>("locales").catch(() => [...navigator.languages]));
translatePage();
wire();
renderPicker("source");
renderPicker("target");
void refreshVolumes();
renderMode();
void validate();
