// The iPhone (phone.rs). Sessions live here, in the island, so the island is
// the one that tells the phone server about them: a snapshot goes to Rust
// whenever it changes, never on a timer.
//
// Switched off, nothing here runs: no State listener, no timer.
//
// The other way round, Rust tells the island what the phone did: a decision
// (its card here closes) or a new queue for a session.

import { Bridge, onEvent } from "../core/bridge";
import { State, type ClaudeSession } from "../core/state";
import { isAway } from "./away";
import { dropApprovalCard, dropQuestionCard } from "./hooks";
import type { Island } from "./island";
import { MAX_QUEUE, setStatusById } from "./sessions";

/** Bursts of changes (a tool call is three events) go out as one snapshot. */
const PUBLISH_DELAY_MS = 150;
/** Steps the phone shows at most. */
const MAX_STEPS = 30;

/** PhoneSession in the API contract. */
export interface PhoneSession {
  id: string;
  name: string;
  cwd: string;
  status: ClaudeSession["status"];
  steps: string[];
  stepIndex: number;
  lastEventAt: number;
  waitingSince: number | null;
  finishedAt: number | null;
  finalMessage: string | null;
  rateResetAt: number | null;
  todo: { done: number; total: number } | null;
  subagents: number;
  summary: ClaudeSession["summary"];
  queue: string[];
}

let unsubscribe: (() => void) | null = null;
let timer: number | null = null;
/** What Rust last got, so an unchanged snapshot is never sent twice. */
let lastSent = "";

function toPhone(s: ClaudeSession): PhoneSession {
  const steps = s.steps.slice(-MAX_STEPS);
  return {
    id: s.id,
    name: s.name,
    cwd: s.cwd,
    status: s.status,
    steps,
    stepIndex: Math.max(0, Math.min(s.stepIndex, steps.length - 1)),
    lastEventAt: s.lastEventAt,
    waitingSince: s.waitingSince,
    finishedAt: s.finishedAt,
    finalMessage: s.finalMessage,
    rateResetAt: s.rateResetAt,
    todo: s.todo,
    subagents: s.subagents.filter((a) => a.doneAt === null).length,
    summary: s.summary,
    queue: s.queue,
  };
}

function publish() {
  timer = null;
  const sessions = State.sessions.map(toPhone);
  const away = isAway();
  const key = JSON.stringify([sessions, away]);
  if (key === lastSent) return;
  lastSent = key;
  void Bridge.phonePublish(sessions, away);
}

function schedule() {
  // A trailing timer, not a reset one: a steady stream of notifies (an open,
  // animating island) still publishes every 150 ms instead of never.
  if (timer == null) timer = window.setTimeout(publish, PUBLISH_DELAY_MS);
}

/**
 * The phone answered a request. If the island has that card up, it goes away
 * the same way it does after a click here; the answer itself is already sent.
 */
function phoneDecided(island: Island, requestId: string) {
  const approval = State.pendingApproval;
  const question = State.pendingQuestion;
  let sessionId: string | null = null;
  if (approval?.requestId === requestId) {
    sessionId = approval.sessionId;
    dropApprovalCard(island);
  } else if (question?.requestId === requestId) {
    sessionId = question.sessionId;
    dropQuestionCard(island);
    State.setPillBadge("integration_claude", null);
    if (State.view === "question") island.setView(State.defaultView());
  }
  if (!sessionId) return;
  void Bridge.log(`phone answered req=${requestId}`);
  setStatusById(sessionId, "working");
  State.notify();
}

/** The phone replaced a session's queue: the island applies it, as if typed here. */
function phoneQueued(sessionId: string, prompts: string[]) {
  const s = State.sessions.find((x) => x.id === sessionId);
  if (!s) return;
  s.queue = prompts.map((p) => p.trim()).filter(Boolean).slice(0, MAX_QUEUE);
  void Bridge.queueSet(s.id, s.queue);
  State.notify();
}

export function registerPhoneHandlers(island: Island) {
  void onEvent<{ requestId: string }>("phone-decision", ({ requestId }) => phoneDecided(island, requestId));
  void onEvent<{ sessionId: string; prompts: string[] }>("phone-queue", ({ sessionId, prompts }) =>
    phoneQueued(sessionId, prompts));
  syncPhone();
}

/** Follows the "Acceso desde el iPhone" setting. */
export function syncPhone() {
  const on = State.settings.phoneEnabled;
  if (on && !unsubscribe) {
    // The server just (re)started with nothing: send everything once.
    lastSent = "";
    unsubscribe = State.subscribe(schedule);
    schedule();
  } else if (!on && unsubscribe) {
    unsubscribe();
    unsubscribe = null;
    if (timer != null) window.clearTimeout(timer);
    timer = null;
  }
}
