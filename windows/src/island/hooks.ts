// Claude Code hook events → island state.
// Port of HookServer.processEvent / processPermissionRequest from the macOS app.
// Difference from macOS: no terminal filter. On Windows the hook fires from any
// terminal (Warp, Windows Terminal, PowerShell…) and all of them are handled.

import { Bridge, onEvent } from "../core/bridge";
import { Sound } from "../core/sound";
import { State, type AskedQuestion, type ClaudeSession } from "../core/state";
import type { Island } from "./island";
import {
  appendStep, endSession, headline, isBusy, makeCurrent, parseResetTime, recordTodos, recordTool,
  resetSummary, setNagHandler, setRateFreeHandler, setRateLimited, setStatus, setStatusById,
  startSubagent, stopSubagent, touchSession,
} from "./sessions";
import { noteActivity } from "./away";

const CLAUDE_ID = "integration_claude";

/** Clears the approval card if no decision was made before the hook gave up. */
let pendingTimeout: number | null = null;
/** Same, for a question the island could answer. */
let questionTimeout: number | null = null;

interface HookPayload {
  hook_event_name?: string;
  request_id?: string;
  session_id?: string;
  cwd?: string;
  message?: string;
  /** UserPromptSubmit carries `prompt`; `message` belongs to Notification. */
  prompt?: string;
  /** Stop carries Claude's final reply of the turn. */
  last_assistant_message?: string;
  /** StopFailure: `rate_limit`, `overloaded`, … and the text Claude Code showed. */
  error_type?: string;
  error_message?: string;
  /** Notification: `idle_prompt`, `quota_auto_resume_fired`, … */
  notification_type?: string;
  tool_name?: string;
  tool_input?: Record<string, unknown>;
  /** Added by coucou-hook on PostToolUse for edits, counted before truncation. */
  coucou_lines?: { added?: number; removed?: number } | null;
  /** Added by coucou-hook: the repo or worktree folder the session works in. */
  coucou_root?: string | null;
  /** Added by coucou-hook: WARP_FOCUS_URL, the session's Warp tab. */
  warp_focus_url?: string | null;
  /** SubagentStart / SubagentStop: which subagent, and what kind (Explore, Plan…). */
  agent_id?: string;
  agent_type?: string;
}

function lastPathComponent(p: string): string {
  const cleaned = p.replace(/[\\/]+$/, "");
  const idx = Math.max(cleaned.lastIndexOf("\\"), cleaned.lastIndexOf("/"));
  return idx >= 0 ? cleaned.slice(idx + 1) : cleaned;
}

/** Step labels, in Spanish (the macOS app shows them in French). */
const TOOL_LABELS: Record<string, string> = {
  Bash: "Ejecuta",
  Read: "Lee",
  Write: "Escribe",
  Edit: "Edita",
  Glob: "Busca",
  Grep: "Busca en",
  WebSearch: "Busca en la web",
  WebFetch: "Descarga",
  TodoWrite: "Tareas",
  Task: "Agente",
  LS: "Lista",
  MultiEdit: "Edita",
  NotebookEdit: "Notebook",
  PowerShell: "Ejecuta",
};

function stepLabel(tool: string, input: Record<string, unknown>): string {
  const label = TOOL_LABELS[tool] ?? tool;
  const str = (k: string) => (typeof input[k] === "string" ? (input[k] as string) : null);
  const cmd = str("command");
  if (cmd) return `${label} · ${cmd.slice(0, 40)}`;
  const path = str("path");
  if (path) return `${label} · ${lastPathComponent(path)}`;
  const file = str("file_path");
  if (file) return `${label} · ${lastPathComponent(file)}`;
  const query = str("query");
  if (query) return `${label} · ${query.slice(0, 40)}`;
  return label;
}

/**
 * What the Allow button actually authorises. Approving "Write" tells you nothing
 * — approving `Write · C:\…\.env` tells you everything, and the difference is
 * the whole point of approving from the island rather than blind.
 *
 * Ordered by how specific the field is, so an unfamiliar tool still shows
 * whatever identifying string it carries instead of falling back to its name.
 */
const APPROVAL_FIELDS = [
  "command", // Bash, PowerShell
  "file_path", // Write, Edit, MultiEdit, NotebookEdit
  "path", // Read, LS
  "url", // WebFetch
  "query", // WebSearch
  "pattern", // Glob, Grep
  "prompt", // Task
] as const;

function approvalTarget(tool: string, input: Record<string, unknown>): string {
  for (const field of APPROVAL_FIELDS) {
    const value = input[field];
    if (typeof value === "string" && value.trim()) {
      return `${tool} · ${value.trim()}`;
    }
  }
  return tool;
}

/** Claude Code's tool for asking the user a question mid-task. */
const ASK_TOOL = "AskUserQuestion";

/** AskUserQuestion carries `questions: [{ question, header, options }]`. */
function firstQuestion(input: Record<string, unknown>): string {
  const list = Array.isArray(input.questions) ? (input.questions as Record<string, unknown>[]) : [];
  const q = list[0]?.question;
  return typeof q === "string" ? q : "Claude tiene una pregunta";
}

/** The questions an AskUserQuestion carries, ready for the island's card. */
function parseQuestions(input: Record<string, unknown>): AskedQuestion[] {
  const str = (v: unknown) => (typeof v === "string" ? v : "");
  const list = Array.isArray(input.questions) ? (input.questions as Record<string, unknown>[]) : [];
  return list
    .map((q) => ({
      question: str(q.question),
      header: str(q.header),
      multiSelect: q.multiSelect === true,
      options: (Array.isArray(q.options) ? (q.options as Record<string, unknown>[]) : [])
        .map((o) => ({ label: str(o.label), description: str(o.description) }))
        .filter((o) => o.label),
    }))
    .filter((q) => q.question && q.options.length > 0);
}

/** The question card stops answering: the terminal has it now. */
export function dropQuestionCard(island: Island) {
  if (questionTimeout != null) window.clearTimeout(questionTimeout);
  questionTimeout = null;
  if (!State.pendingQuestion) return;
  State.pendingQuestion = null;
  State.isPinned = false;
  island.dropPin();
}

export function registerHookHandlers(island: Island) {
  setNagHandler((session) => nag(island, session));
  setRateFreeHandler((session) => rateFreed(island, session));
  void onEvent<HookPayload>("hook", (payload) => handleHook(island, payload));
}

/**
 * A session has been waiting on you for too long: Mochi fidgets and chimes,
 * and the island comes out of hiding so you notice.
 */
function nag(island: Island, session: ClaudeSession) {
  if (State.paused) return;
  void Bridge.log(`nag ${session.name} (${session.status})`);
  // Show the session that needs you, unless an approval card already owns the pill.
  makeCurrent(session.id);
  if (State.focusId !== CLAUDE_ID) State.setPillBadge(CLAUDE_ID, "approval");
  // Only a pending question chimes; a permission reminder stays silent. Either
  // way Mochi knocks on the glass, and a permission gets its card opened.
  island.nudge(session.status === "question", session.status === "approval");
  State.notify();
}

/** The limit reset: Mochi cheers up and says so, then the island folds away. */
function rateFreed(island: Island, session: ClaudeSession) {
  if (State.paused) return;
  setStatus(session, "idle");
  Sound.play("pop", "rateFree");
  island.announce(`Ya se liberó el límite de uso · ${session.name}`);
  State.notify();
}

/** Says a session's limit reset, from Claude Code's own auto-resume notice. */
const RESUMED = "quota_auto_resume_fired";

function isRateLimitText(text: string): boolean {
  const lower = text.toLowerCase();
  return lower.includes("rate limit") || lower.includes("usage limit") ||
    lower.includes("limit reached") || lower.includes("hit your") ||
    lower.includes("limite d") || lower.includes("límite");
}

/** Lowers the approval card and hands the pill back. */
function dropApprovalCard(island: Island) {
  State.pendingApproval = null;
  State.isPinned = false;
  island.dropPin();
  State.setPillBadge(CLAUDE_ID, null);
  if (State.view === "approval") island.setView(State.defaultView());
}

function handleHook(island: Island, payload: HookPayload) {
  if (State.paused) {
    // Silence here used to cost Claude Code nearly two minutes: the relay waited
    // for a decision from an island that had already decided not to look. Say so,
    // and the terminal takes the question immediately.
    if (payload.request_id) void Bridge.approvalDecline(payload.request_id);
    return;
  }

  const name = payload.hook_event_name ?? "";
  const cwd = payload.cwd ?? "";
  const focused = State.focusId === CLAUDE_ID;

  if (name === "SessionEnd") {
    if (payload.session_id) endSession(payload.session_id);
    State.notify();
    return;
  }

  const session = touchSession(
    payload.session_id || "default", cwd, payload.coucou_root, payload.warp_focus_url,
  );
  /** Nothing else is holding the pill: no approval card, no other busy session on it. */
  const pillFree = () =>
    !State.pendingApproval &&
    (!State.pendingQuestion || State.pendingQuestion.sessionId === session.id) &&
    (State.currentSessionId === session.id || !isBusy(State.currentSession));

  // The session moved on — answered in the terminal, or closed: the island's
  // question card has nothing left to answer.
  const pq = State.pendingQuestion;
  if (pq && pq.sessionId === session.id && name !== "PermissionRequest" && name !== "PreToolUse" &&
    name !== "Notification") {
    dropQuestionCard(island);
  }

  /** Alerts force the island open; work events only reveal the compact island. */
  const surface = (view: Parameters<Island["alert"]>[0], isAlert: boolean) => {
    if (State.mode === "expanded") {
      if (isAlert) island.setView(view);
    } else if (isAlert) {
      island.alert(view);
    } else if (State.mode === "hidden") {
      island.reveal();
    }
  };

  /**
   * Claude asked you something and won't go on until you answer, in the terminal.
   * Chimes once per question; the reminder takes it from there.
   */
  const asks = (question: string) => {
    const already = session.status === "question";
    setStatus(session, "question");
    if (question) appendStep(session, question.slice(0, 120));
    if (already) return;
    Sound.play("question", "question");
    const free = pillFree();
    if (free) makeCurrent(session.id);
    if (focused && free) {
      surface("question", true);
    } else {
      State.setPillBadge(CLAUDE_ID, "approval");
      island.reveal();
    }
  };

  /** The session hit its usage limit: the amber card, with when it comes back. */
  const limited = (text: string) => {
    const already = session.status === "ratelimit";
    setRateLimited(session, parseResetTime(text));
    if (already) return;
    appendStep(session, "Límite de uso");
    Sound.play("rate", "rate");
    const free = pillFree();
    if (free) makeCurrent(session.id);
    if (focused && free) {
      surface("ratelimit", true);
    } else {
      State.setPillBadge(CLAUDE_ID, "approval");
      island.reveal();
    }
  };

  switch (name) {
    case "SessionStart":
      setStatus(session, "idle");
      surface("overview", false);
      Sound.play("work", "start");
      break;

    case "UserPromptSubmit": {
      // You just typed into this one: it's the one to watch.
      makeCurrent(session.id);
      resetSummary(session);
      setStatus(session, "thinking");
      // The field is `prompt`; reading `message` meant this step was always blank.
      const asked = payload.prompt ?? payload.message;
      if (asked) appendStep(session, asked.slice(0, 60));
      // The island's peek chime confirms your prompt reached Coucou.
      Sound.play("peek", "prompt");
      surface("overview", false);
      break;
    }

    case "PreToolUse": {
      const tool = payload.tool_name ?? "Tool";
      if (tool === ASK_TOOL) {
        asks(firstQuestion(payload.tool_input ?? {}));
        break;
      }
      setStatus(session, "working");
      if (tool === "TodoWrite") recordTodos(session, payload.tool_input ?? {});
      appendStep(session, stepLabel(tool, payload.tool_input ?? {}));
      surface("overview", false);
      break;
    }

    case "PostToolUse":
    case "PostToolUseFailure": {
      const ok = name === "PostToolUse";
      recordTool(session, payload.tool_name ?? "", payload.tool_input ?? {}, ok, payload.coucou_lines);
      setStatus(session, "working");
      if (!ok) appendStep(session, "⚠ falló");
      break;
    }

    case "Notification": {
      const message = payload.message ?? "";
      if (payload.notification_type === RESUMED) {
        // Claude Code picked the task back up by itself: the limit is gone.
        if (session.status === "ratelimit") setStatus(session, "working");
      } else if (isRateLimitText(message)) {
        limited(message);
      } else if (message.endsWith("?")) {
        asks(message);
      }
      // "Claude is waiting for your input" is ignored on purpose: it only means
      // the turn ended, which the Stop event already announced.
      break;
    }

    case "Stop": {
      // Any subagent still out is done by now: send them all home.
      while (session.subagents.some((a) => a.doneAt == null)) stopSubagent(session, undefined);
      setStatus(session, "finished");
      // Stop has no `message`: without the reply the card fell back to the last
      // tool step, and a turn "ended" on `Ejecuta · cd C:/Users/…`.
      const said = headline(payload.last_assistant_message ?? "");
      session.finalMessage = said || null;
      if (said) appendStep(session, said.slice(0, 60));
      Sound.play("finish", "finish");
      // Show the finished card for this session — unless an approval card is up,
      // or you're watching another session that is still working.
      const free = pillFree();
      if (free) makeCurrent(session.id);
      if (focused && free) {
        surface("finished", true);
        if (session.summary.files.length > 0) island.celebrate();
      } else {
        State.setPillBadge(CLAUDE_ID, "finished");
      }
      break;
    }

    case "StopFailure": {
      if (payload.error_type === "rate_limit") {
        limited(payload.error_message ?? payload.message ?? "");
        break;
      }
      setStatus(session, "error");
      Sound.play("error", "error");
      const free = pillFree();
      if (free) makeCurrent(session.id);
      if (focused && free) surface("error", true);
      else State.setPillBadge(CLAUDE_ID, "error");
      break;
    }

    case "SubagentStart": {
      const type = payload.agent_type || "subagente";
      startSubagent(session, payload.agent_id || `${Date.now()}`, type);
      appendStep(session, `+ subagente · ${type}`);
      break;
    }

    case "SubagentStop":
      stopSubagent(session, payload.agent_id);
      appendStep(session, "• subagente listo");
      break;

    case "PermissionRequest": {
      const requestId = payload.request_id ?? "";
      // A question isn't a permission: the card offers its options instead of
      // Allow/Deny, and the chosen ones go back as the tool's answers. While it
      // waits the terminal doesn't show the question; "Responder en la terminal"
      // or the timeout hands it over.
      if (payload.tool_name === ASK_TOOL) {
        const questions = parseQuestions(payload.tool_input ?? {});
        const taken = State.pendingApproval ||
          (State.pendingQuestion && State.pendingQuestion.requestId !== requestId);
        if (!requestId || taken || questions.length === 0) {
          if (requestId) void Bridge.approvalDecline(requestId);
          asks(firstQuestion(payload.tool_input ?? {}));
          break;
        }
        if (questionTimeout != null) window.clearTimeout(questionTimeout);
        State.pendingQuestion = { requestId, sessionId: session.id, questions, step: 0, answers: {} };
        makeCurrent(session.id, true);
        void Bridge.approvalAck(requestId);
        asks(questions[0].question);
        State.isPinned = true;
        if (focused) {
          island.alert("question");
        } else {
          State.setPillBadge(CLAUDE_ID, "approval");
          island.reveal();
        }
        questionTimeout = window.setTimeout(() => {
          questionTimeout = null;
          if (State.pendingQuestion?.requestId !== requestId) return;
          // Still a question — in the terminal now.
          dropQuestionCard(island);
          State.notify();
        }, 110_000);
        break;
      }
      // One card, one request. A second one must never quietly replace the first
      // — that would leave a human staring at request B while request A waits for
      // a decision nobody can give. Hand it straight back to the terminal.
      if ((State.pendingApproval && State.pendingApproval.requestId !== requestId) || State.pendingQuestion) {
        if (requestId) void Bridge.approvalDecline(requestId);
        break;
      }
      if (pendingTimeout != null) window.clearTimeout(pendingTimeout);
      const tool = payload.tool_name ?? "Tool";
      const input = payload.tool_input ?? {};
      State.pendingApproval = {
        requestId,
        sessionId: session.id,
        tool,
        command: approvalTarget(tool, input),
      };
      // The card names the session it answers for.
      makeCurrent(session.id, true);
      // The relay's short ack window closes in 800 ms; everything below this
      // line is synchronous, so the card really is up by the time it lands.
      if (requestId) void Bridge.approvalAck(requestId);
      setStatus(session, "approval");
      State.isPinned = true;
      Sound.play("approval", "approval");
      if (focused) {
        island.alert("approval");
      } else {
        // Another agent holds the view, so the card would yank it away. The badge
        // is the signal instead — but it has to be on screen for that to mean
        // anything, hence the reveal. We just told the relay a human can act.
        State.setPillBadge(CLAUDE_ID, "approval");
        island.reveal();
      }
      // Coucou answers within 108 s or not at all; after that the terminal has
      // taken over and the card would be lying.
      pendingTimeout = window.setTimeout(() => {
        pendingTimeout = null;
        const pending = State.pendingApproval;
        if (!pending) return;
        dropApprovalCard(island);
        // Still waiting on you — in the terminal now.
        setStatusById(pending.sessionId, "approval");
        State.notify();
      }, 110_000);
      break;
    }

    default:
      break;
  }

  // Work is happening: a good moment to check whether the user is still here.
  noteActivity(island);
  State.notify();
}
