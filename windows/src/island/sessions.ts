// Claude Code sessions — several can run at once, usually one per worktree.
//
// Every hook event carries its session_id, so each session keeps its own steps,
// status and summary here. The Warp pill (`integration_claude`) mirrors whichever
// session is "current": the one you last typed into, or the one that needs you.
// The Sessions view lists them all.
//
// Nothing here polls. Waiting reminders are one setTimeout per waiting session,
// set when it starts waiting and cleared the moment it moves on, so a hidden
// island still costs nothing.

import type { BotStateName } from "../core/layout";
import { State, type ClaudeSession, type SessionStatus, type SessionSummary } from "../core/state";

const CLAUDE_ID = "integration_claude";

/** How long the finished pose lasts before Mochi settles back to idle. */
const FINISHED_POSE_MS = 5200;
/** A session that hasn't sent anything for this long has been closed without a SessionEnd. */
const STALE_MS = 3 * 60 * 60 * 1000;
/** Longest session name shown; worktree names can be long. */
const MAX_NAME = 24;
/** Reminders per wait, so a session you deliberately left doesn't nag forever. */
const MAX_NAGS = 3;

/** Blocked until you act: a permission to grant or a question to answer. */
const WAITING: ReadonlySet<SessionStatus> = new Set(["approval", "question"]);

const nagTimers = new Map<string, number>();
const nagCounts = new Map<string, number>();
let finishTimer: number | null = null;

/** Called when a session has been waiting on the user for too long. */
let onNag: ((s: ClaudeSession) => void) | null = null;

export function setNagHandler(fn: (s: ClaudeSession) => void) {
  onNag = fn;
}

// ── Naming ────────────────────────────────────────────────────────────────────

/**
 * `C:\repo\.claude\worktrees\cvj-ver-documento-da\windows` → `cvj-ver-documento-da`.
 * Inside a worktree folder (`worktrees`, `.worktrees`, `repo-worktrees`) the
 * worktree's own name wins over whichever subfolder the session started in.
 */
export function sessionBaseName(cwd: string): string {
  const parts = cwd.split(/[\\/]+/).filter(Boolean);
  for (let i = parts.length - 2; i >= 0; i--) {
    const p = parts[i].toLowerCase();
    if (p === "worktrees" || p === ".worktrees" || p.endsWith("-worktrees")) return parts[i + 1];
  }
  return parts.at(-1) ?? "sesión";
}

function shorten(name: string): string {
  return name.length > MAX_NAME ? `${name.slice(0, MAX_NAME - 1)}…` : name;
}

/** Two sessions in the same folder get `name`, `name·2`, … */
function uniqueName(base: string): string {
  const taken = new Set(State.sessions.map((s) => s.name));
  const short = shorten(base);
  if (!taken.has(short)) return short;
  for (let i = 2; ; i++) {
    const candidate = `${short}·${i}`;
    if (!taken.has(candidate)) return candidate;
  }
}

// ── Lifecycle ─────────────────────────────────────────────────────────────────

function emptySummary(): SessionSummary {
  return { files: [], added: 0, removed: 0, tests: "none" };
}

/** Finds the session behind an event, creating it on first sight. */
export function touchSession(id: string, cwd: string, root?: string | null): ClaudeSession {
  const now = Date.now();
  pruneStale(now);
  let s = State.sessions.find((x) => x.id === id);
  if (!s) {
    s = {
      id,
      // The relay names the repo or worktree; the folder heuristic is for an
      // older relay, or a session outside any git repo.
      name: uniqueName(root || sessionBaseName(cwd)),
      cwd,
      status: "idle",
      steps: [],
      stepIndex: 0,
      lastEventAt: now,
      finishedAt: null,
      waitingSince: null,
      summary: emptySummary(),
    };
    State.sessions.push(s);
  }
  s.lastEventAt = now;
  if (cwd) s.cwd = cwd;
  // Most recent first: that's the order the Sessions view lists them in.
  State.sessions.sort((a, b) => b.lastEventAt - a.lastEventAt);
  if (!State.currentSession) State.currentSessionId = s.id;
  return s;
}

export function endSession(id: string) {
  clearNag(id);
  State.sessions = State.sessions.filter((s) => s.id !== id);
  if (State.currentSessionId === id) State.currentSessionId = State.sessions[0]?.id ?? null;
  mirror();
}

function pruneStale(now: number) {
  for (const s of State.sessions) {
    if (now - s.lastEventAt > STALE_MS && !WAITING.has(s.status)) clearNag(s.id);
  }
  State.sessions = State.sessions.filter((s) => now - s.lastEventAt <= STALE_MS || WAITING.has(s.status));
  if (State.currentSessionId && !State.currentSession) {
    State.currentSessionId = State.sessions[0]?.id ?? null;
  }
}

/**
 * Puts a session on the Warp pill. Refused while another session's approval
 * card is up: the card would otherwise show one name and answer for another.
 */
export function makeCurrent(id: string, force = false): boolean {
  const approval = State.pendingApproval;
  if (!force && approval && approval.sessionId && approval.sessionId !== id) return false;
  if (!State.sessions.some((s) => s.id === id)) return false;
  State.currentSessionId = id;
  mirror();
  return true;
}

/** True when the session is in the middle of something you'd want to watch. */
export function isBusy(s: ClaudeSession | null): boolean {
  return !!s && s.status !== "idle" && s.status !== "finished" && s.status !== "error";
}

// ── Status ────────────────────────────────────────────────────────────────────

export function setStatus(s: ClaudeSession, status: SessionStatus) {
  const wasWaiting = WAITING.has(s.status);
  s.status = status;
  s.finishedAt = status === "finished" ? Date.now() : null;
  if (WAITING.has(status)) {
    if (!wasWaiting || s.waitingSince == null) {
      s.waitingSince = Date.now();
      nagCounts.set(s.id, 0);
      scheduleNag(s);
    }
  } else {
    s.waitingSince = null;
    clearNag(s.id);
  }
  if (status === "finished") scheduleFinishPose();
  mirror();
}

export function setStatusById(id: string, status: SessionStatus) {
  const s = State.sessions.find((x) => x.id === id);
  if (s) setStatus(s, status);
}

export function appendStep(s: ClaudeSession, step: string) {
  s.steps.push(step);
  if (s.steps.length > 20) s.steps.shift();
  s.stepIndex = s.steps.length - 1;
  mirror();
}

/** The finished pose ends on its own; one shared timer re-mirrors when it does. */
function scheduleFinishPose() {
  if (finishTimer != null) window.clearTimeout(finishTimer);
  finishTimer = window.setTimeout(() => {
    finishTimer = null;
    mirror();
    State.notify();
  }, FINISHED_POSE_MS);
}

function botState(s: ClaudeSession): BotStateName {
  switch (s.status) {
    case "finished":
      return s.finishedAt != null && Date.now() - s.finishedAt < FINISHED_POSE_MS ? "finished" : "idle";
    default:
      return s.status;
  }
}

/** Copies the current session onto the Warp pill. */
export function mirror() {
  const t = State.tasks.find((x) => x.id === CLAUDE_ID);
  if (!t) return;
  const s = State.currentSession;
  if (!s) {
    t.name = "Warp";
    t.steps = [];
    t.stepIndex = 0;
    t.state = "idle";
    t.sessionCwd = null;
    t.sessionId = null;
    return;
  }
  t.name = s.name;
  t.steps = s.steps;
  t.stepIndex = s.stepIndex;
  t.state = botState(s);
  t.sessionCwd = s.cwd;
  t.sessionId = s.id;
}

// ── Waiting reminders ─────────────────────────────────────────────────────────

function clearNag(id: string) {
  const timer = nagTimers.get(id);
  if (timer != null) window.clearTimeout(timer);
  nagTimers.delete(id);
  nagCounts.delete(id);
}

function scheduleNag(s: ClaudeSession) {
  const timer = nagTimers.get(s.id);
  if (timer != null) window.clearTimeout(timer);
  nagTimers.delete(s.id);
  const minutes = State.settings.waitingAlertMinutes;
  if (!(minutes > 0) || s.waitingSince == null) return;
  const count = nagCounts.get(s.id) ?? 0;
  if (count >= MAX_NAGS) return;
  const due = s.waitingSince + minutes * 60_000 * (count + 1);
  nagTimers.set(
    s.id,
    window.setTimeout(() => {
      nagTimers.delete(s.id);
      const live = State.sessions.find((x) => x.id === s.id);
      if (!live || !WAITING.has(live.status)) return;
      nagCounts.set(live.id, count + 1);
      onNag?.(live);
      scheduleNag(live);
    }, Math.max(1000, due - Date.now())),
  );
}

/** The reminder delay changed in settings: re-arm every waiting session. */
export function rescheduleNags() {
  for (const s of State.sessions) {
    if (WAITING.has(s.status)) scheduleNag(s);
  }
}

/**
 * "desde 9:41" — when the session started waiting. A clock time never goes
 * stale, so nothing has to tick to keep it true.
 */
export function waitingSinceLabel(s: ClaudeSession): string {
  if (s.waitingSince == null) return "";
  const time = new Date(s.waitingSince).toLocaleTimeString("es-MX", { hour: "numeric", minute: "2-digit" });
  return `desde ${time}`;
}

// ── Summary ───────────────────────────────────────────────────────────────────

const EDIT_TOOLS: ReadonlySet<string> = new Set(["Write", "Edit", "MultiEdit", "NotebookEdit"]);
const SHELL_TOOLS: ReadonlySet<string> = new Set(["Bash", "PowerShell"]);

/** Test runners, the common ones. A command matching this counts as "ran tests". */
const TEST_COMMAND =
  /(^|[\s;&|(])(npm|pnpm|yarn|bun)\s+(run\s+)?test\b|\b(pytest|jest|vitest|mocha|phpunit|rspec|ctest|tox|nox)\b|\b(cargo|go|dotnet|deno|bun|mix|swift|flutter|dart)\s+test\b|\bcargo\s+nextest\b|\bplaywright\s+test\b|\bcypress\s+run\b|\b(gradlew?|mvn)\b.*\btest\b|\bunittest\b/i;

export function isTestCommand(command: string): boolean {
  return TEST_COMMAND.test(command);
}

/** Same multiset line diff as coucou-hook, for a relay too old to send `coucou_lines`. */
function lineDiff(oldText: string, newText: string): [number, number] {
  const lines = (t: string) => (t ? t.replace(/\r\n/g, "\n").replace(/\n$/, "").split("\n") : []);
  const counts = new Map<string, number>();
  for (const l of lines(oldText)) counts.set(l, (counts.get(l) ?? 0) + 1);
  let added = 0;
  for (const l of lines(newText)) {
    const c = counts.get(l) ?? 0;
    if (c > 0) counts.set(l, c - 1);
    else added++;
  }
  let removed = 0;
  for (const c of counts.values()) removed += c;
  return [added, removed];
}

function fallbackLines(tool: string, input: Record<string, unknown>): [number, number] {
  const str = (o: Record<string, unknown>, k: string) => (typeof o[k] === "string" ? (o[k] as string) : "");
  switch (tool) {
    case "Write":
      return lineDiff("", str(input, "content"));
    case "Edit":
      return lineDiff(str(input, "old_string"), str(input, "new_string"));
    case "NotebookEdit":
      return lineDiff("", str(input, "new_source"));
    case "MultiEdit": {
      const edits = Array.isArray(input.edits) ? (input.edits as Record<string, unknown>[]) : [];
      return edits.reduce<[number, number]>((acc, e) => {
        const [a, r] = lineDiff(str(e, "old_string"), str(e, "new_string"));
        return [acc[0] + a, acc[1] + r];
      }, [0, 0]);
    }
    default:
      return [0, 0];
  }
}

/** Starts a fresh summary: it covers what happened since the last prompt. */
export function resetSummary(s: ClaudeSession) {
  s.summary = emptySummary();
}

/** PostToolUse (ok) / PostToolUseFailure (!ok) → files, lines, tests. */
export function recordTool(
  s: ClaudeSession,
  tool: string,
  input: Record<string, unknown>,
  ok: boolean,
  lines?: { added?: number; removed?: number } | null,
) {
  if (EDIT_TOOLS.has(tool) && ok) {
    const file = input.file_path ?? input.notebook_path;
    if (typeof file === "string" && file && !s.summary.files.includes(file)) s.summary.files.push(file);
    const [added, removed] =
      lines && typeof lines.added === "number"
        ? [lines.added, lines.removed ?? 0]
        : fallbackLines(tool, input);
    s.summary.added += added;
    s.summary.removed += removed;
  }
  if (SHELL_TOOLS.has(tool) && typeof input.command === "string" && isTestCommand(input.command)) {
    // One failing run marks the whole turn: a later green run doesn't erase it.
    if (!ok) s.summary.tests = "failed";
    else if (s.summary.tests === "none") s.summary.tests = "passed";
  }
}

/** "3 archivos · +120 −14 · tests ✓" — empty for a turn that changed nothing. */
export function summaryText(summary: SessionSummary): string {
  const parts: string[] = [];
  const n = summary.files.length;
  if (n > 0) {
    parts.push(n === 1 ? "1 archivo" : `${n} archivos`);
    parts.push(`+${summary.added} −${summary.removed}`);
  }
  if (summary.tests === "passed") parts.push("tests ✓");
  else if (summary.tests === "failed") parts.push("tests ✗");
  return parts.join(" · ");
}

// ── Labels ────────────────────────────────────────────────────────────────────

export const STATUS_LABELS: Record<SessionStatus, string> = {
  idle: "Inactiva",
  thinking: "Pensando",
  working: "Trabajando",
  approval: "Espera permiso",
  question: "Te pregunta",
  finished: "Terminó",
  error: "Error",
  ratelimit: "Límite de uso",
};

export const STATUS_COLORS: Record<SessionStatus, string> = {
  idle: "#6B7079",
  thinking: "#A78BFA",
  working: "#3B9EFF",
  approval: "#F5A524",
  question: "#22D3EE",
  finished: "#34D399",
  error: "#F4505E",
  ratelimit: "#F59E0B",
};

export function isWaiting(s: ClaudeSession): boolean {
  return WAITING.has(s.status);
}
