// "You're away" — focus mode while Claude keeps working without you, and the
// "Mientras no estabas" card when you come back.
//
// Nothing polls while you're here. The idle time is checked when Claude Code
// does something (a hook event, at most every 15 s); only once you're away does
// a slow 5 s check run, to notice you coming back, and it stops right then.

import { Bridge } from "../core/bridge";
import { State, type AwayRow, type SessionStatus } from "../core/state";
import type { Island } from "./island";
import { STATUS_LABELS, isBusy, summaryText } from "./sessions";

const CHECK_EVERY_MS = 15_000;
const WHILE_AWAY_MS = 5_000;
/** Back means input in the last few seconds. */
const BACK_IDLE_S = 5;

/** Worth telling you about on your return. */
const NEWS: ReadonlySet<SessionStatus> = new Set(["finished", "error", "approval", "question", "ratelimit"]);

let away = false;
let awaySince = 0;
let lastCheck = 0;
let checking = false;
let timer: number | null = null;
let focusOn = false;

/** True once the user has been idle past the absence interval, until they're back. */
export function isAway(): boolean {
  return away;
}

/** Called on every hook event. */
export function noteActivity(island: Island) {
  syncFocus(island);
  const now = Date.now();
  if (now - lastCheck < CHECK_EVERY_MS) return;
  lastCheck = now;
  void check(island);
}

/** The cursor touched the island: certainly back. */
export function noteUserHere(island: Island) {
  if (away) back(island);
}

async function check(island: Island) {
  if (checking || State.paused) return;
  checking = true;
  try {
    const idle = await Bridge.idleSeconds();
    if (idle == null) return;
    if (!away && idle >= State.settings.absenceInterval) {
      away = true;
      awaySince = Date.now() - idle * 1000;
      timer = window.setInterval(() => void check(island), WHILE_AWAY_MS);
    } else if (away && idle < BACK_IDLE_S) {
      back(island);
    }
    syncFocus(island);
  } finally {
    checking = false;
  }
}

/** Headphones on while you're away and a session is busy; off (with a stretch) when it's done. */
function syncFocus(island: Island) {
  const working = State.sessions.some((s) => isBusy(s) && (s.status === "working" || s.status === "thinking"));
  if (away && working && !focusOn) {
    focusOn = true;
    island.setFocusMode(true);
  } else if (focusOn && (!working || !away)) {
    focusOn = false;
    island.setFocusMode(false, !working);
  }
}

function back(island: Island) {
  away = false;
  if (timer != null) window.clearInterval(timer);
  timer = null;
  if (focusOn) {
    focusOn = false;
    island.setFocusMode(false, true);
  }
  const rows: AwayRow[] = State.sessions
    .filter((s) => s.lastEventAt >= awaySince && NEWS.has(s.status))
    .map((s) => ({
      sessionId: s.id,
      name: s.name,
      status: s.status,
      detail: [STATUS_LABELS[s.status], summaryText(s.summary) || s.finalMessage || s.steps.at(-1) || ""]
        .filter(Boolean)
        .join(" · "),
    }));
  if (rows.length === 0) return;
  // Whatever needs you first, then what failed, then what finished.
  const rank = (st: SessionStatus) => (st === "approval" || st === "question" ? 0 : st === "error" || st === "ratelimit" ? 1 : 2);
  rows.sort((a, b) => rank(a.status) - rank(b.status));
  State.away = { rows, minutes: Math.max(1, Math.round((Date.now() - awaySince) / 60_000)) };
  island.showAway();
}
