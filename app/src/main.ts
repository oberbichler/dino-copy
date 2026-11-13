// DinoCopy, the desktop app for dino-copy: pick source and target, press Start, watch it run.

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
  /** Free and total bytes, as last seen. */
  free?: number;
  total?: number;
  /** A remembered disk that is not connected right now. */
  offline?: boolean;
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
  /** Done work over all work (bytes, plus a share per file), 0 to 1. */
  fraction: number;
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

/** A group of problems, in the order of importance (see run.rs). */
type IssueKind =
  | "targetFull"
  | "readOnly"
  | "noPermission"
  | "tooLarge"
  | "badName"
  | "nameClash"
  | "unreadable"
  | "changed"
  | "other";

interface Issue {
  kind: IssueKind;
  count: number;
}

interface RunResult {
  mode: Mode;
  outcome: "success" | "inSync" | "scanned" | "incomplete" | "failed" | "cancelled";
  headline: Msg;
  /** What went wrong, grouped. */
  issues: Issue[];
  /** Further notes (deletions skipped, ...). */
  messages: Msg[];
  /** Every problem as a technical line. */
  details: string[];
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
  if (choice?.offline) detail.textContent = t("offline");
  el.querySelector(".disk-icon")!.className = `disk-icon ${choice?.kind ?? "none"}`;
  el.classList.toggle("empty", !choice);
  el.classList.toggle("offline", choice?.offline === true);
}

async function openPicker(side: Side): Promise<void> {
  const el = $(`pick-${side}`);
  el.classList.add("active");
  // Read fresh every time: a disk plugged in a moment ago must show up.
  const volumes = await invoke<Volume[]>("list_volumes");
  const current = state[side];
  const otherSide: Side = side === "source" ? "target" : "source";
  const other = state[otherSide];
  const items: MenuItems = [];
  for (const v of volumes) {
    // The disk on the other side cannot be picked here as well: it says
    // which side it is on instead.
    if (other?.path === v.path) {
      items.push({ text: `${v.name}  ·  ${t(otherSide)}`, enabled: false });
      continue;
    }
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
  $("run-current").hidden = false;
  $("run-issues").hidden = true;
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
  // The backend measures the work so that the bar and the estimate agree.
  $("run-bar").style.width = `${Math.min(100, p.fraction * 100).toFixed(1)}%`;
  $("run-eta").textContent = p.etaSecs === null ? t("estimating") : fmt.timeLeft(p.etaSecs);
  $("run-bytes").textContent = t("xOfY", { done: fmt.bytes(p.bytesDone), total: fmt.bytes(p.bytesTotal) });
  $("run-rate").textContent = p.bytesPerSec > 0 ? `${fmt.bytes(p.bytesPerSec)}/s` : "–";
  $("run-files").textContent = t("xOfY", { done: p.filesDone, total: p.filesTotal });
  setPath($("run-current"), p.current);
}

/** Problems as they happen: a full target should not wait for the end. */
function onIssues(kinds: IssueKind[]): void {
  const el = $("run-issues");
  el.hidden = kinds.length === 0;
  el.textContent = kinds.map((k) => issueTitle(k)).join(" · ");
  // The room of the current file name, which matters less now.
  $("run-current").hidden = kinds.length > 0;
}

// ---------------------------------------------------------------------------
// 3 · Done, 4 · Scan
// ---------------------------------------------------------------------------

const ISSUE_KEY = {
  targetFull: "TargetFull",
  readOnly: "ReadOnly",
  noPermission: "NoPermission",
  tooLarge: "TooLarge",
  badName: "BadName",
  nameClash: "NameClash",
  unreadable: "Unreadable",
  changed: "Changed",
  other: "Other",
} as const satisfies Record<IssueKind, string>;

function issueTitle(kind: IssueKind): string {
  return t(`issue${ISSUE_KEY[kind]}`);
}

function issueText(issue: Issue): string {
  return t(`issue${ISSUE_KEY[issue.kind]}Text`, { n: issue.count });
}

/** Title and one sentence for the result, short and in plain words. */
function doneText(r: RunResult): { title: string; text: string } {
  const h = r.headline;
  switch (r.outcome) {
    case "success":
      return { title: msgText(h), text: summaryText(r) };
    case "inSync":
      return { title: t("inSyncTitle"), text: summaryText(r) };
    case "incomplete":
      return { title: t("notAllMirrored"), text: summaryText(r) };
    case "cancelled":
      return { title: t("cancelledTitle"), text: msgText(h) };
    default:
      return h.key === "sourceEmpty"
        ? { title: t("sourceEmptyTitle"), text: msgText(h) }
        : { title: t("failedTitle"), text: msgText(h) };
  }
}

async function onDone(r: RunResult): Promise<void> {
  state.running = false;
  state.lastResult = r;
  if (r.mode === "mirror") void notify(doneText(r).title);
  if (state.closeWhenDone) {
    await win.destroy();
    return;
  }
  if (r.outcome === "scanned" && r.items) {
    await showScan(r);
    return;
  }
  const { title, text } = doneText(r);
  const problems = r.issues.length + r.messages.length > 0;
  $("done-icon").className = `done-icon ${r.outcome}`;
  $("done-headline").textContent = title;
  $("done-summary").textContent = text;
  $("done-body").classList.toggle("has-issues", problems);
  renderIssues($("done-issues"), r.issues, r.messages);

  // Details: the technical lines, to look up or to pass on.
  $("done-details").textContent = r.details.join("\n");
  showDetails(false);
  $("details-toggle").hidden = r.details.length === 0;

  // The next step, where there is one: try again, or carry on.
  const retry = $("done-retry");
  retry.hidden = !(r.outcome === "incomplete" || r.outcome === "cancelled" || (r.outcome === "failed" && r.headline.key !== "sourceEmpty"));
  retry.textContent = t(r.outcome === "cancelled" ? "resume" : "retry");

  show("done");
  // One problem fits the small window; more need room to be read.
  await setWindow(r.issues.length + r.messages.length > 1 ? "tall" : "small");
}

/** The technical lines take the place of the issues while they are shown. */
function showDetails(open: boolean): void {
  $("done-details").hidden = !open;
  $("done-issues").hidden = open;
  $("details-copy").hidden = !open;
  $("details-copy").textContent = t("copyDetails");
  $("details-toggle").setAttribute("aria-expanded", String(open));
}

/** Issues as title and advice; further notes in plain text below. */
function renderIssues(list: HTMLElement, issues: Issue[], notes: Msg[]): void {
  list.replaceChildren();
  for (const issue of issues) {
    const li = document.createElement("li");
    li.className = `issue ${issue.kind}`;
    const title = document.createElement("b");
    title.textContent = issueTitle(issue.kind);
    const text = document.createElement("p");
    text.textContent = issueText(issue);
    li.append(title, text);
    list.append(li);
  }
  for (const m of notes) {
    const li = document.createElement("li");
    li.className = "note";
    li.textContent = msgText(m);
    list.append(li);
  }
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
  $("scan-note").textContent = t("unchanged", { n: s.unchangedCount });
  // What a mirror could not do, in one line above the list.
  const notes = [...r.issues.map((i) => `${issueTitle(i.kind)} (${fmt.count(i.count)})`), ...r.messages.map(msgText)];
  $("scan-issues").hidden = notes.length === 0;
  $("scan-issues").textContent = notes.join(" · ");
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
  if (await isPermissionGranted()) sendNotification({ title: "DinoCopy", body });
}

/** Puts text on the clipboard (with a fallback for older web views). */
async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
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
  $("done-retry").addEventListener("click", () => void start("mirror"));
  $("details-toggle").addEventListener("click", () => showDetails($("done-details").hidden === true));
  $("details-copy").addEventListener("click", async () => {
    await copyText($("done-details").textContent ?? "");
    $("details-copy").textContent = t("detailsCopied");
  });
  void win.onFocusChanged(({ payload: focused }) => {
    if (focused) void refreshVolumes();
  });
  // A disk was plugged in or ejected while the window is open.
  void listen("volumes://changed", () => void refreshVolumes());
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
  void listen<IssueKind[]>("run://issues", (e) => onIssues(e.payload));
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

/**
 * Remembered disks: current free space, or "not connected". Runs at start,
 * whenever the window comes to the front, and whenever a disk is plugged in
 * or ejected.
 */
async function refreshVolumes(): Promise<void> {
  if (state.running) return;
  const volumes = await invoke<Volume[] | null>("list_volumes").catch(() => null);
  if (!volumes) return;
  for (const side of ["source", "target"] as const) {
    const choice = state[side];
    if (choice?.kind !== "volume") continue;
    const v = volumes.find((v) => v.path === choice.path);
    state[side] = v ? { ...choice, free: v.availableBytes, total: v.totalBytes, offline: false } : { ...choice, offline: true };
    localStorage.setItem(side, JSON.stringify(state[side]));
    renderPicker(side);
  }
  if (!$("setup").hidden) void validate();
}

setLanguage(await invoke<string[]>("locales").catch(() => [...navigator.languages]));
translatePage();
wire();
renderPicker("source");
renderPicker("target");
void refreshVolumes();
renderMode();
void validate();
