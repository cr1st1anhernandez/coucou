// App state — mirror of AppState.swift (the parts the island needs).

import type { BotEmoteName, BotStateName, IslandMode, IslandViewName } from "./layout";
import type { EyeShape } from "../mochi/engine";

export type AgentSource = "claudeCode" | "n8n";
export type PillBadge = "approval" | "finished" | "error";

export interface AgentTask {
  id: string;
  name: string;
  color: string;
  state: BotStateName;
  stepIndex: number;
  steps: string[];
  source: AgentSource;
  isIntegration: boolean;
  emote?: BotEmoteName | null;
  miniEye?: EyeShape | null;
  pillBadge?: PillBadge | null;
  sessionCwd?: string | null;
  /** The Claude Code session the Warp pill mirrors right now. */
  sessionId?: string | null;
  /** That session's Warp tab, `warp://session/<id>`, when it runs in Warp. */
  sessionFocusUrl?: string | null;
  /** The session's to-do list progress (TodoWrite), for the ring around Mochi. */
  todo?: TodoProgress | null;
}

export interface TodoProgress {
  done: number;
  total: number;
}

/** A subagent Claude launched; it shows as a mini Mochi beside Mochi. */
export interface Subagent {
  id: string;
  type: string;
  /** Date.now() when it finished; it flies back into Mochi shortly after. */
  doneAt: number | null;
}

/** Where a Claude Code session stands, as shown in the Sessions view. */
export type SessionStatus =
  | "idle" | "thinking" | "working" | "approval" | "question"
  | "finished" | "error" | "ratelimit";

/** One edited file and the lines it gained and lost this turn. */
export interface FileChange {
  path: string;
  added: number;
  removed: number;
}

/** What a session did since its last prompt — shown when it finishes. */
export interface SessionSummary {
  /** In the order they were first touched. */
  files: FileChange[];
  added: number;
  removed: number;
  tests: "none" | "passed" | "failed";
}

/** One Claude Code session (one per worktree, usually), keyed by session_id. */
export interface ClaudeSession {
  id: string;
  /** Short name from the worktree folder, e.g. `cvj-ver-documento-da`. */
  name: string;
  cwd: string;
  status: SessionStatus;
  steps: string[];
  stepIndex: number;
  /** Date.now() of the last hook event. */
  lastEventAt: number;
  /** Date.now() of the Stop event, so Mochi only celebrates for a moment. */
  finishedAt: number | null;
  /** Date.now() since when the session has been waiting on the user. */
  waitingSince: number | null;
  summary: SessionSummary;
  /** First sentence of Claude's last reply, the finished card's title. */
  finalMessage: string | null;
  /** Warp's deep link to the session's tab (WARP_FOCUS_URL), if it runs in Warp. */
  focusUrl: string | null;
  /** Date.now() at which a rate-limited session's limit resets, when Claude said. */
  rateResetAt: number | null;
  todo: TodoProgress | null;
  subagents: Subagent[];
  /** Prompts you queued from the island, sent one each time a turn ends. */
  queue: string[];
}

/** One line of the "Mientras no estabas" card. */
export interface AwayRow {
  sessionId: string;
  name: string;
  status: SessionStatus;
  detail: string;
}

export interface ApprovalInfo {
  requestId: string;
  sessionId: string;
  tool: string;
  command: string;
}

/** One option of an AskUserQuestion question. */
export interface QuestionOption {
  label: string;
  description: string;
}

/** One question of an AskUserQuestion call. */
export interface AskedQuestion {
  question: string;
  header: string;
  options: QuestionOption[];
  multiSelect: boolean;
}

/** An AskUserQuestion the island can answer, one question at a time. */
export interface PendingQuestion {
  requestId: string;
  sessionId: string;
  questions: AskedQuestion[];
  /** Index of the question on screen. */
  step: number;
  /** Question text → chosen label(s), joined with ", " for a multi-select. */
  answers: Record<string, string>;
}

export interface ChatMessage {
  id: number;
  role: "user" | "assistant";
  content: string;
  /** Name of the file sent with this message. */
  attachment?: string;
  /** What Claude is doing while the reply is still empty ("Leyendo el archivo…"). */
  status?: string;
}

/** A file waiting to go out with the next message of a chat. */
export interface Attachment {
  name: string;
  path: string;
}

/** The library's ways of taking a prompt to the terminal (at least one is on). */
export interface PasteModes {
  copy: boolean;
  warp: boolean;
  paste: boolean;
}

export type PromptContext =
  | { kind: "window"; appName: string; title: string; url?: string }
  | { kind: "file"; name: string; path?: string };

export interface ResultItem {
  label: string;
  detail: string;
  url?: string;
}

export interface SearchResult {
  title: string;
  items: ResultItem[];
  note?: string;
}

const task = (
  id: string, name: string, color: string, source: AgentSource,
): AgentTask => ({
  id, name, color, state: "idle", stepIndex: 0, steps: [], source, isIntegration: true,
});

/** AgentTask.integrationAgents — same ids, names and colours as macOS. */
export const INTEGRATION_AGENTS: AgentTask[] = [
  task("integration_claude", "Warp", "#F5F6F8", "claudeCode"),
  task("integration_github", "GitHub", "#F4505E", "n8n"),
];

export const TOGGLEABLE_INTEGRATION_IDS = ["integration_github"];

/** What an integration poller last reported. */
export interface IntegrationInfo {
  data: Record<string, unknown>;
  error: string | null;
  loaded: boolean;
  configured: boolean;
}

export interface Settings {
  soundEnabled: boolean;
  soundVolume: number;
  autoCloseInterval: number;
  absenceInterval: number;
  activeIntegrations: string[];
  screen: "primary" | "cursor";
  autostart: boolean;
  hooksInstalled: boolean;
  /** Claude model used by the chat. */
  model: string;
  /** Minutes a session may wait on you before Mochi fidgets and chimes. 0 = off. */
  waitingAlertMinutes: number;
  /** Per-cue sound switches (SOUND_CUES); a missing cue uses its default. */
  soundCues: Record<string, boolean>;
  /** The chat runs on the user's Claude Code (their account) or on an API key. */
  chatEngine: "claudeCode" | "api";
  pasteModes: PasteModes;
}

/** Seconds the open island waits after the mouse leaves before closing. */
export const AUTO_CLOSE_CHOICES = [2, 3, 5, 10];

export const DEFAULT_SETTINGS: Settings = {
  soundEnabled: true,
  soundVolume: 0.12,
  autoCloseInterval: 5,
  absenceInterval: 180,
  activeIntegrations: ["integration_github"],
  screen: "primary",
  autostart: false,
  hooksInstalled: false,
  model: "claude-opus-5",
  waitingAlertMinutes: 2,
  soundCues: {},
  chatEngine: "claudeCode",
  pasteModes: { copy: true, warp: true, paste: false },
};

type Listener = () => void;

class AppState {
  mode: IslandMode = "hidden";
  view: IslandViewName = "overview";

  tasks: AgentTask[] = [];
  focusId: string | null = null;

  stateOverride: BotStateName | null = null;

  /** Cursor in logical screen pixels, origin top-left (like AppState.mousePosition). */
  mouse = { x: 0, y: 0 };
  /** Cursor relative to the island's top-left corner. */
  mouseInIsland = { x: 0, y: 0 };

  isPinned = false;
  paused = false;

  uploadProgress = 0;
  uploadDuration = 2.4;
  fileDragOver = false;
  /** A library file is being dragged out of the island: its own drag isn't a drop on us. */
  libraryDragOut = false;

  promptContext: PromptContext | null = null;
  droppedFile: { name: string; path: string } | null = null;
  noteMessage: string | null = null;
  searchResult: SearchResult | null = null;
  chatHistory: ChatMessage[] = [];
  /** Files picked or dropped onto a chat, waiting for its next message. */
  attachments: { chat: Attachment | null; library: Attachment | null } = { chat: null, library: null };
  pendingApproval: ApprovalInfo | null = null;
  pendingQuestion: PendingQuestion | null = null;

  /** What happened while the user was away, and for how long they were gone. */
  away: { rows: AwayRow[]; minutes: number } | null = null;

  /** Live Claude Code sessions, most recent activity first. */
  sessions: ClaudeSession[] = [];
  currentSessionId: string | null = null;

  integrations: Record<string, IntegrationInfo> = {};

  lastActivity = performance.now();

  settings: Settings = { ...DEFAULT_SETTINGS };

  private listeners = new Set<Listener>();

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Marks the UI dirty; the island re-renders on the next frame. */
  notify() {
    for (const fn of this.listeners) fn();
  }

  get focusTask(): AgentTask | null {
    return this.tasks.find((t) => t.id === this.focusId) ?? this.tasks[0] ?? null;
  }

  get currentSession(): ClaudeSession | null {
    return this.sessions.find((s) => s.id === this.currentSessionId) ?? null;
  }

  get effectiveState(): BotStateName {
    return this.stateOverride ?? this.focusTask?.state ?? "idle";
  }

  get otherTasks(): AgentTask[] {
    return this.tasks.filter((t) => t.id !== this.focusId);
  }

  setFocus(id: string) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    this.focusId = id;
    t.pillBadge = null;
    this.notify();
  }

  updateTask(id: string, state: BotStateName) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.state = state;
    this.notify();
  }

  appendStep(id: string, step: string) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.steps.push(step);
    if (t.steps.length > 20) t.steps.shift();
    t.stepIndex = t.steps.length - 1;
    this.notify();
  }

  setPillBadge(id: string, badge: PillBadge | null) {
    const t = this.tasks.find((x) => x.id === id);
    if (!t) return;
    t.pillBadge = badge;
    this.notify();
  }

  /** loadIntegrationTasks() — Warp (Claude Code) always on, the rest opt-in (max 4). */
  loadIntegrationTasks() {
    for (const proto of INTEGRATION_AGENTS) {
      const shouldLoad =
        proto.id === "integration_claude" || this.settings.activeIntegrations.includes(proto.id);
      const idx = this.tasks.findIndex((t) => t.id === proto.id);
      if (shouldLoad && idx < 0) this.tasks.push({ ...proto, steps: [] });
      if (!shouldLoad && idx >= 0) this.tasks.splice(idx, 1);
    }
    // Keep the declared order so pills never shuffle.
    const order = INTEGRATION_AGENTS.map((t) => t.id);
    this.tasks.sort((a, b) => order.indexOf(a.id) - order.indexOf(b.id));
    if (!this.focusId) this.focusId = "integration_claude";
    this.notify();
  }

  toggleIntegration(id: string) {
    if (id === "integration_claude") return;
    const active = this.settings.activeIntegrations;
    if (active.includes(id)) {
      this.settings.activeIntegrations = active.filter((x) => x !== id);
      if (this.focusId === id) this.focusId = "integration_claude";
    } else {
      if (active.length >= 4) return;
      this.settings.activeIntegrations = [...active, id];
    }
    this.loadIntegrationTasks();
  }

  defaultView(): IslandViewName {
    return this.tasks.length === 0 ? "empty" : "overview";
  }
}

export const State = new AppState();
