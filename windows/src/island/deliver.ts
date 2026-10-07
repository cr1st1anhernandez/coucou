// Queued prompts for a session that is not working.
//
// The Stop hook hands a busy session its next queued prompt when the turn ends
// (pipe.rs). A session that already ended its turn sends no more Stops, so a
// prompt queued for it — from the iPhone, typically, with the user away — would
// wait forever. Instead Rust types it into the session's terminal and presses
// Enter, one prompt at a time; the rest follow on the Stops after it.
//
// Only for a session that is idle, finished or in error, with no approval or
// question open (a rate-limited one waits for its Stop). Rust adds the last
// condition: nobody is typing at the PC. Nothing runs unless a session like
// that has a queue; a retry timer exists only while one does.
//
// Typed is not yet received: if the session's UserPromptSubmit doesn't follow
// within CONFIRM_MS, the text is most likely still in the input box with its
// Enter lost, so Enter is pressed once more. Another CONFIRM_MS without it and
// the prompt goes back to the front of the queue, and out of the input box.
//
// When a prompt can't be delivered the phones hear why (`queue-error`), once
// per prompt and reason: the retries that follow don't repeat it.

import { Bridge, type Delivery, type QueueError } from "../core/bridge";
import { State, type ClaudeSession } from "../core/state";
import { isAway } from "./away";
import { MAX_QUEUE, appendStep, resetSummary, setStatus, takeQueued } from "./sessions";

/** Statuses in which Claude Code sits at its prompt, waiting for one. */
const AT_PROMPT: ReadonlySet<ClaudeSession["status"]> = new Set(["idle", "finished", "error"]);
/** Someone is at the PC: ask again after this. */
const RETRY_MS = 15_000;
/** The terminal couldn't be reached: ask again, but rarely. */
const FAILED_RETRY_MS = 60_000;
/** A typed prompt has this long to show up as a UserPromptSubmit. */
const CONFIRM_MS = 10_000;

/** Sessions with a delivery under way. */
const inFlight = new Set<string>();
/** Session → earliest time to ask again after a refusal. */
const retryAt = new Map<string, number>();
let timer: number | null = null;
/** Session → the last `queue-error` the phones heard for it (reason + prompt). */
const reported = new Map<string, string>();
/** Session → the prompt just typed into it, until its UserPromptSubmit. */
const unconfirmed = new Map<
  string,
  { prompt: string; status: ClaudeSession["status"]; timer: number; retried: boolean }
>();

function ready(s: ClaudeSession): boolean {
  return (
    s.queue.length > 0 &&
    AT_PROMPT.has(s.status) &&
    State.pendingApproval?.sessionId !== s.id &&
    State.pendingQuestion?.sessionId !== s.id
  );
}

function check() {
  if (State.paused) return;
  const now = Date.now();
  let next = Infinity;
  for (const s of State.sessions) {
    if (!ready(s)) {
      retryAt.delete(s.id);
      reported.delete(s.id);
      continue;
    }
    if (inFlight.has(s.id) || unconfirmed.has(s.id)) continue;
    const at = retryAt.get(s.id) ?? 0;
    if (at > now) next = Math.min(next, at);
    else void deliver(s);
  }
  if (next < Infinity && timer == null) {
    timer = window.setTimeout(() => {
      timer = null;
      check();
    }, next - now);
  }
}

async function deliver(s: ClaudeSession) {
  const prompt = s.queue[0];
  inFlight.add(s.id);
  const result: Delivery = (await Bridge.queueDeliver(s.id, prompt, isAway())) ?? "failed";
  inFlight.delete(s.id);
  void Bridge.log(`queue deliver ${s.name}: ${result}`);
  if (result === "sent") {
    retryAt.delete(s.id);
    reported.delete(s.id);
    const status = s.status;
    const timer = window.setTimeout(() => void unanswered(s.id), CONFIRM_MS);
    unconfirmed.set(s.id, { prompt, status, timer, retried: false });
    takeQueued(s, prompt);
    // Same as a Stop that carried a queued prompt: the session is on it now,
    // and its UserPromptSubmit is on the way.
    resetSummary(s);
    setStatus(s, "thinking");
    appendStep(s, `↻ ${prompt.slice(0, 58)}`);
  } else {
    const wait = result === "user-active" || result === "stale" ? RETRY_MS : FAILED_RETRY_MS;
    retryAt.set(s.id, Date.now() + wait);
    if (result === "no-terminal" || result === "failed") report(s.id, result, prompt);
  }
  State.notify();
}

/**
 * No UserPromptSubmit after a typed prompt: Enter once more, the first time;
 * after that it goes back to the front of the queue.
 */
async function unanswered(sessionId: string) {
  const pending = unconfirmed.get(sessionId);
  if (pending && !pending.retried) {
    pending.retried = true;
    const pressed = (await Bridge.queuePressEnter(sessionId, isAway())) ?? false;
    // The prompt may have arrived while Enter was on its way.
    if (unconfirmed.get(sessionId) !== pending) return;
    if (pressed) {
      pending.timer = window.setTimeout(() => void unanswered(sessionId), CONFIRM_MS);
      return;
    }
  }
  unconfirmed.delete(sessionId);
  const s = State.sessions.find((x) => x.id === sessionId);
  if (!pending || !s) return;
  void Bridge.log(`queue deliver ${s.name}: no UserPromptSubmit, prompt back in the queue`);
  report(s.id, "not-received", pending.prompt);
  // Before anything that could run check(): no instant retype.
  retryAt.set(s.id, Date.now() + FAILED_RETRY_MS);
  s.queue = [pending.prompt, ...s.queue].slice(0, MAX_QUEUE);
  void Bridge.queueSet(s.id, s.queue);
  // It is back in the queue, so it must not also wait in the input box: an
  // Enter at the PC would send it now, and the queue again later.
  void Bridge.queueClearInput(s.id, isAway());
  // Undo the "thinking" the delivery assumed, unless something else happened.
  if (s.status === "thinking") setStatus(s, pending.status);
  State.notify();
}

/** Tells the phones, unless they already heard this same reason for this prompt. */
function report(sessionId: string, reason: QueueError, prompt: string) {
  const key = `${reason}\n${prompt}`;
  if (reported.get(sessionId) === key) return;
  reported.set(sessionId, key);
  void Bridge.phoneQueueError(sessionId, reason, prompt);
}

/** hooks.ts, on every UserPromptSubmit: a typed prompt made it. */
export function promptArrived(sessionId: string) {
  const pending = unconfirmed.get(sessionId);
  if (!pending) return;
  window.clearTimeout(pending.timer);
  unconfirmed.delete(sessionId);
}

export function registerDelivery() {
  State.subscribe(check);
  check();
}
