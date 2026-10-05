// Island views — DOM ports of IslandViewContent.swift. Paddings, font sizes,
// colours and wording are copied from the Swift views so both platforms read
// identically.

import { h, svg, clear, dot } from "./dom";
import { ICONS } from "./icons";
import { Ticker } from "./ticker";
import { AUTO_CLOSE_CHOICES, State, type AgentTask, type AskedQuestion } from "../core/state";
import { MAX_CARD_ROWS, washRGBA, type BotEmoteName, type IslandViewName, type Wash } from "../core/layout";
import { createMiniBot, pruneMiniBots } from "../mochi/minibots";
import { buildPrompt } from "./chat";
import { buildLibrary } from "./library";
import { buildChoose, buildUpload, buildUploading } from "./upload";
import { renderIntegrationCard, type IntegrationCardHooks } from "./integrations";
import {
  MAX_QUEUE, STATUS_COLORS, STATUS_LABELS, isBusy, isWaiting, queuePrompt, rateCountdown,
  rateResetLabel, summaryText, unqueuePrompt, waitingSinceLabel,
} from "../island/sessions";

export interface ViewActions {
  setView(v: IslandViewName): void;
  collapse(): void;
  setFocus(id: string): void;
  openTerminal(): void;
  /** The ↗ button: opens whatever the focused pill points at. */
  openTarget(): void;
  openUrl(url: string): void;
  decide(d: "allow" | "deny"): void;
  /** Answers the question on the card; the last one sends them all. */
  answerQuestion(labels: string[]): void;
  /** Gives the question back to the terminal. */
  questionToTerminal(): void;
  toggleSound(): void;
  setVolume(v: number): void;
  setAutoClose(seconds: number): void;
  openSettingsWindow(): void;
  blip(): void;
  /** Puts a Claude Code session on the Warp pill and shows it. */
  selectSession(id: string): void;
  /** A quick reaction from Mochi (a wink when something is copied…). */
  emote(e: BotEmoteName): void;
}

export interface ViewHost {
  el: HTMLElement;
  sync(): void;
  /** Called when the view becomes active, for views with a text field. */
  focus?(): void;
  /** Called every frame while the view is on screen. */
  tick?(nowMs: number): void;
  /** True while the view is mid-animation and needs more frames to finish it. */
  animating?(): boolean;
}

// ── Shared pieces ─────────────────────────────────────────────────────────────

function card(wash: Wash, ...children: (Node | string)[]): HTMLElement {
  const el = h("div", { class: wash ? "card wash" : "card" }, ...children);
  if (wash) el.style.setProperty("--wash", washRGBA(wash));
  return el;
}

function btn(
  label: string,
  kind: "primary" | "secondary",
  onClick: () => void,
  kbd?: string,
): HTMLElement {
  return h(
    "button",
    { class: `btn ${kind}`, onclick: onClick },
    h("span", { text: label }),
    kbd ? h("span", { class: "kbd", text: kbd }) : null,
  );
}

/** AgentWho — coloured dot + task name + grey label. */
function agentWho(task: AgentTask | null, label: string): HTMLElement {
  const row = h("div", { class: "who-row" });
  if (task) {
    row.append(dot(task.color, 8), h("span", { class: "n", text: task.name }));
  }
  row.append(h("span", { text: label }));
  return row;
}

function stack(padLeft: number, padRight: number, ...children: Node[]): HTMLElement {
  const el = h("div", { class: "stack" }, ...children);
  el.style.padding = `4px ${padRight}px 4px ${padLeft}px`;
  return el;
}

// ── Header ────────────────────────────────────────────────────────────────────

export function buildHeader(actions: ViewActions): ViewHost {
  const tabHome = h("button", { class: "tab", title: "Inicio", onclick: () => go("overview") }, svg(ICONS.house, 13));
  const tabChat = h("button", { class: "tab", title: "Preguntar", onclick: () => go("prompt") }, svg(ICONS.bubble, 13));
  const tabLibrary = h("button", { class: "tab", title: "Biblioteca", onclick: () => go("library") }, svg(ICONS.books, 13));
  const sessionCount = h("span", { class: "tab-count" });
  const tabSessions = h(
    "button",
    { class: "tab", title: "Sesiones", onclick: () => go("sessions") },
    svg(ICONS.stack, 13),
    sessionCount,
  );

  const gearBtn = h("button", { title: "Ajustes", onclick: () => go("settings") }, svg(ICONS.gear, 14));
  const soundBtn = h("button", { title: "Silenciar", onclick: () => actions.toggleSound() }, svg(ICONS.speakerOn, 14));

  function go(v: IslandViewName) {
    actions.blip();
    actions.setView(v);
  }

  const el = h(
    "div",
    { id: "header" },
    h("div", { class: "tabs" }, tabHome, tabSessions, tabChat, tabLibrary),
    h("div", { class: "header-actions" }, gearBtn, soundBtn),
  );

  return {
    el,
    sync() {
      const v = State.view;
      tabHome.classList.toggle("on", v === "overview" || v === "empty");
      tabChat.classList.toggle("on", v === "prompt");
      tabLibrary.classList.toggle("on", v === "library");
      tabSessions.classList.toggle("on", v === "sessions");
      // Only what needs you: a permission or a question. It drops as you answer.
      const pending = State.sessions.filter(isWaiting).length;
      sessionCount.textContent = pending > 0 ? String(pending) : "";
      const open = State.sessions.length;
      tabSessions.title = open === 1 ? "Sesiones · 1 abierta" : `Sesiones · ${open} abiertas`;
      gearBtn.classList.toggle("on", v === "settings");
      clear(gearBtn);
      gearBtn.append(svg(v === "settings" ? ICONS.gearFill : ICONS.gear, 14));
      clear(soundBtn);
      soundBtn.append(svg(State.settings.soundEnabled ? ICONS.speakerOn : ICONS.speakerOff, 14));
      el.style.opacity = v === "confused" ? "0" : "1";
    },
  };
}

// ── Overview ──────────────────────────────────────────────────────────────────

function buildOverview(actions: ViewActions): ViewHost {
  const ticker = new Ticker();
  const who = h("div", { class: "who" });
  const tickerBody = h("div", { class: "card-body" }, who, ticker.el);
  const leftBody = h("div", { class: "left-body" });
  const jump = h(
    "button",
    { class: "icon-btn jump", title: "Abrir", onclick: () => actions.openTarget() },
    svg(ICONS.arrowUpRight, 8),
  );
  const left = card(null, leftBody, jump);
  const pills = h("div", { class: "pills" });
  const right = card(null, pills);

  const el = h("div", { class: "view overview" },
    h("div", { class: "left" }, left),
    h("div", { class: "right" }, right),
  );

  let pillIds = "";
  let detailOpen = false;
  let lastFocus: string | null = null;
  let mode: "ticker" | "card" | null = null;
  let cardKey = "";
  let tickerSession: string | null = null;

  const hooks: IntegrationCardHooks = {
    get detailOpen() {
      return detailOpen;
    },
    openDetail() {
      detailOpen = true;
      cardKey = "";
      State.notify();
    },
    closeDetail() {
      detailOpen = false;
      cardKey = "";
      State.notify();
    },
    openSettings: () => actions.openSettingsWindow(),
  };

  return {
    el,
    tick(nowMs: number) {
      if (mode === "ticker") ticker.tick(nowMs);
    },
    animating: () => mode === "ticker" && ticker.animating,
    sync() {
      const task = State.focusTask;
      if (task?.id !== lastFocus) {
        lastFocus = task?.id ?? null;
        detailOpen = false;
        cardKey = "";
        mode = null;
      }

      // The Warp pill with a live Claude Code session keeps the ticker; every other
      // pill shows its own card, exactly like IntegrationCardView.
      const sessionActive =
        task?.id === "integration_claude" &&
        (task.state !== "idle" || task.steps.length > 0 || State.sessions.length > 0);

      if (task && sessionActive) {
        if (mode !== "ticker") {
          clear(leftBody);
          leftBody.append(tickerBody);
          mode = "ticker";
          cardKey = "";
        }
        if ((task.sessionId ?? null) !== tickerSession) {
          tickerSession = task.sessionId ?? null;
          ticker.reset();
        }
        // While Claude works you can already line up what comes next. The button
        // takes the "Claude Code" label's place, so the session name keeps its room.
        const session = task.id === "integration_claude" ? State.currentSession : null;
        const queueable = !!session && (isBusy(session) || session.queue.length > 0);
        clear(who);
        who.append(
          dot(task.color, 7),
          h("span", { class: "name", text: task.name }),
          queueable ? "" : h("span", { class: "tool", text: task.source === "claudeCode" ? "Claude Code" : "n8n" }),
        );
        const others = State.sessions.length - 1;
        if (task.id === "integration_claude" && others > 0) {
          const waiting = State.sessions.some((s) => s.id !== task.sessionId && isWaiting(s));
          who.append(h("button", {
            class: waiting ? "more-sessions alert" : "more-sessions",
            title: "Ver todas las sesiones",
            text: `+${others}`,
            onclick: () => actions.setView("sessions"),
          }));
        }
        if (session && queueable) {
          const n = session.queue.length;
          who.append(h("button", {
            class: n > 0 ? "queue-btn on" : "queue-btn",
            title: "Prompts en cola: se envían cuando Claude termine",
            text: n > 0 ? `${n} en cola` : "+ cola",
            onclick: () => actions.setView("queue"),
          }));
        }
        if (task.steps.length > 1) {
          who.append(h("span", {
            class: "count",
            text: `${Math.min(task.stepIndex + 1, task.steps.length)}/${task.steps.length}`,
          }));
        }
        ticker.sync(task);
      } else if (task) {
        const info = State.integrations[task.id];
        const key = [
          task.id, detailOpen, task.state, task.steps.join("|"),
          info?.loaded, info?.error, info?.configured,
          JSON.stringify(info?.data ?? {}),
        ].join("~");
        if (key !== cardKey) {
          cardKey = key;
          mode = "card";
          clear(leftBody);
          leftBody.append(renderIntegrationCard(task, hooks));
        }
      }

      jump.style.display = detailOpen ? "none" : "";

      const others = State.otherTasks.slice(0, 4);
      const pillKey = others.map((t) => `${t.id}:${t.pillBadge ?? ""}`).join("|");
      if (pillKey !== pillIds) {
        pillIds = pillKey;
        clear(pills);
        for (const t of others) pills.append(buildPill(t, actions));
        pruneMiniBots();
      }
    },
  };
}

function buildPill(task: AgentTask, actions: ViewActions): HTMLElement {
  const label = task.name;
  const canvas = createMiniBot(task, 24);
  const pill = h(
    "div",
    { class: "pill", onclick: () => actions.setFocus(task.id) },
    canvas,
    h("span", { class: "lbl", text: label }),
  );
  pill.style.borderColor = `${task.color}24`;
  pill.addEventListener("mouseenter", () => {
    pill.style.background = `${task.color}2e`;
    pill.style.borderColor = `${task.color}8c`;
    pill.style.boxShadow = `0 2px 10px ${task.color}59`;
    (pill.querySelector(".lbl") as HTMLElement).style.color = lighten(task.color, 0.3);
  });
  pill.addEventListener("mouseleave", () => {
    pill.style.background = "";
    pill.style.borderColor = `${task.color}24`;
    pill.style.boxShadow = "";
    (pill.querySelector(".lbl") as HTMLElement).style.color = "";
  });

  if (task.pillBadge) {
    const colors = { approval: "#F5A524", finished: "#22C55E", error: "#F4505E" } as const;
    const icons = { approval: ICONS.bang, finished: ICONS.check, error: ICONS.xmark } as const;
    const inner = h("i", { style: `background:${colors[task.pillBadge]}` }, svg(icons[task.pillBadge], 6, { stroke: task.pillBadge === "finished" ? 3 : 0 }));
    const badge = h("div", { class: "pill-badge" }, inner);
    badge.style.boxShadow = `0 0 4px ${colors[task.pillBadge]}99`;
    pill.append(badge);
  }
  return pill;
}

function lighten(hex: string, amount: number): string {
  const v = parseInt(hex.replace("#", ""), 16);
  const c = [(v >> 16) & 255, (v >> 8) & 255, v & 255].map((x) =>
    Math.min(255, Math.round(x + amount * 255)),
  );
  return `rgb(${c[0]},${c[1]},${c[2]})`;
}

// ── Empty ─────────────────────────────────────────────────────────────────────

function buildEmpty(actions: ViewActions): ViewHost {
  const body = h(
    "div",
    { class: "stack", style: "padding:0 18px 0 118px;flex-direction:row;align-items:center;gap:16px" },
    h(
      "div",
      { style: "display:flex;flex-direction:column;gap:5px" },
      h("div", { class: "title", text: "No hay nada corriendo ahorita." }),
      h("div", { class: "sub", text: "Suelta un archivo o pregúntame lo que quieras." }),
    ),
    h("div", { class: "grow" }),
    btn("Preguntar a Claude", "primary", () => actions.setView("prompt")),
  );
  return { el: h("div", { class: "view" }, card(null, body)), sync() {} };
}

// ── Approval ──────────────────────────────────────────────────────────────────

function buildApproval(actions: ViewActions): ViewHost {
  const who = h("div");
  const code = h("div", { class: "code" });
  const row = h("div", { class: "actions" });
  const el = h("div", { class: "view" }, card("amber", stack(116, 16, who, code, row)));
  let rowKey = "";
  return {
    el,
    sync() {
      clear(who);
      who.append(agentWho(State.focusTask, "necesita permiso"));
      // The whole point of approving here rather than in the terminal: this line
      // is the command, the file path or the URL being authorised, not just the
      // name of the tool asking.
      code.textContent = State.pendingApproval?.command || State.pendingApproval?.tool || "…";
      // Two buttons, built once. Rebuilding them between a mouse-down and a
      // mouse-up would swallow the click, and there is nothing left to vary:
      // "Always" is gone until the remembered-rules list exists to back it.
      if (rowKey === "built") return;
      rowKey = "built";
      clear(row);
      row.append(
        btn("Denegar", "secondary", () => actions.decide("deny"), "N"),
        btn("Permitir", "primary", () => actions.decide("allow"), "S"),
      );
    },
  };
}

// ── Question ──────────────────────────────────────────────────────────────────

/**
 * Claude asks something. When the island holds the request, the options are
 * rows to click, like the result card's; a multi-select ticks them and sends
 * with Enviar. Otherwise the question can only be answered in the terminal.
 */
function buildQuestion(actions: ViewActions): ViewHost {
  const who = h("div");
  const title = h("div", { class: "title two-lines" });
  const options = h("div", { class: "res" });
  const row = h("div", { class: "actions" });
  const body = stack(116, 16, who, title, options, row);
  const el = h("div", { class: "view" }, card("cyan", body));
  let key = "";
  let picked = new Set<string>();

  function draw(q: AskedQuestion, step: number, total: number) {
    clear(options);
    for (const o of q.options) {
      const on = picked.has(o.label);
      const r = h("button", {
        class: on ? "res-row option on" : "res-row option",
        title: o.description || o.label,
        onclick: () => {
          if (!q.multiSelect) return actions.answerQuestion([o.label]);
          if (picked.has(o.label)) picked.delete(o.label);
          else picked.add(o.label);
          draw(q, step, total);
        },
      },
        q.multiSelect ? h("i", { class: on ? "tick on" : "tick" }, on ? svg(ICONS.check, 7, { stroke: 3 }) : null) : null,
        h("b", { text: o.label }),
        h("span", { text: o.description }),
      );
      options.append(r);
    }
    clear(row);
    if (q.multiSelect) {
      const send = btn(step + 1 < total ? "Siguiente" : "Enviar", "primary", () => {
        if (picked.size > 0) actions.answerQuestion(q.options.map((o) => o.label).filter((l) => picked.has(l)));
      });
      if (picked.size === 0) send.classList.add("off");
      row.append(send);
    }
    row.append(btn("Responder en la terminal", "secondary", () => actions.questionToTerminal()));
  }

  return {
    el,
    sync() {
      const pq = State.pendingQuestion;
      const q = pq && pq.sessionId === State.currentSessionId ? pq.questions[pq.step] : null;
      const next = q && pq ? `${pq.requestId}:${pq.step}` : `plain:${State.focusTask?.steps.at(-1) ?? ""}`;
      if (next === key) return;
      key = next;
      picked = new Set();
      clear(who);
      if (q && pq) {
        const total = pq.questions.length;
        who.append(agentWho(State.focusTask, total > 1 ? `te pregunta · ${pq.step + 1}/${total}` : "te pregunta"));
        if (q.header) who.firstElementChild?.append(h("span", { class: "chip", text: q.header }));
        title.textContent = q.question;
        options.style.display = "";
        body.classList.add("list");
        draw(q, pq.step, total);
        return;
      }
      who.append(agentWho(State.focusTask, "Claude Code te hace una pregunta"));
      title.textContent = State.focusTask?.steps.at(-1) ?? "Claude necesita una respuesta.";
      options.style.display = "none";
      body.classList.remove("list");
      clear(row);
      row.append(h("div", { class: "sub", text: "Responde en tu terminal." }));
    },
  };
}

// ── Error ─────────────────────────────────────────────────────────────────────

function buildError(actions: ViewActions): ViewHost {
  const who = h("div");
  const title = h("div", { class: "title", text: "La integración reportó un error." });
  const detail = h("div", { class: "detail" });
  const row = h("div", { class: "actions" },
    btn("Reintentar", "primary", () => actions.setView(State.defaultView())),
    btn("Abrir", "secondary", () => actions.openTarget()),
  );
  const el = h("div", { class: "view" }, card("red", stack(116, 16, who, title, detail, row)));
  return {
    el,
    sync() {
      const task = State.focusTask;
      clear(who);
      who.append(agentWho(task, task?.source === "n8n" ? "Integración" : "Claude Code"));
      title.textContent = task?.source === "n8n" ? "La integración reportó un error." : "La sesión se detuvo por un error.";
      detail.textContent = task?.steps.at(-1) ?? "Sin detalles.";
    },
  };
}

// ── Rate limit ────────────────────────────────────────────────────────────────

/** The session hit its usage limit: when it comes back, counting down. */
function buildRateLimit(actions: ViewActions): ViewHost {
  const who = h("div");
  const title = h("div", { class: "title", text: "Llegaste al límite de uso." });
  const when = h("div", { class: "summary" });
  const row = h("div", { class: "actions" },
    btn("Abrir terminal", "primary", () => actions.openTerminal()),
    btn("OK", "secondary", () => actions.collapse()),
  );
  const el = h("div", { class: "view" }, card("amber", stack(116, 16, who, title, when, row)));
  return {
    el,
    sync() {
      clear(who);
      who.append(agentWho(State.focusTask, "Claude Code"));
      const session = State.currentSession;
      when.textContent = session ? rateResetLabel(session) : "";
    },
  };
}

// ── Finished ──────────────────────────────────────────────────────────────────

/** `C:\repo\src\main.ts` → `main.ts`. */
function fileName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}

/**
 * The prototype's result card, for a turn that changed files: Claude's last
 * line, one row per file with its +/−, then the totals. A turn that touched
 * nothing keeps the plain card.
 */
function buildFinished(actions: ViewActions): ViewHost {
  const who = h("div");
  const title = h("div", { class: "title one-line" });
  const rows = h("div", { class: "res" });
  const summary = h("div", { class: "summary" });
  const row = h("div", { class: "actions" },
    btn("Abrir terminal", "primary", () => actions.openTerminal()),
    btn("OK", "secondary", () => actions.collapse()),
  );
  const body = stack(116, 16, who, title, rows, summary, row);
  const el = h("div", { class: "view" }, card("green", body));
  let key = "";
  return {
    el,
    sync() {
      clear(who);
      who.append(agentWho(State.focusTask, "Claude Code terminó"));
      const session = State.currentSession;
      title.textContent = session?.finalMessage || "Sesión terminada";
      const files = session?.summary.files ?? [];
      const shown = files.slice(0, MAX_CARD_ROWS);
      const next = shown.map((f) => `${f.path}:${f.added}:${f.removed}`).join("|");
      if (next !== key) {
        key = next;
        clear(rows);
        for (const f of shown) {
          rows.append(h("div", { class: "res-row", title: f.path },
            h("b", { text: fileName(f.path) }),
            h("span", { text: `+${f.added} −${f.removed}` }),
          ));
        }
      }
      rows.style.display = shown.length > 0 ? "" : "none";
      body.classList.toggle("list", shown.length > 0);
      const more = files.length - shown.length;
      const totals = session ? summaryText(session.summary) : "";
      summary.textContent = more > 0 ? `${totals} · ${more} más` : totals;
      summary.classList.toggle("fine", shown.length > 0);
    },
  };
}

// ── Away ──────────────────────────────────────────────────────────────────────

/**
 * "Mientras no estabas": back at the keyboard, what moved while you were gone —
 * what needs you first, then what failed, then what finished. A row takes you
 * to that session (or straight to its permission card).
 */
function buildAway(actions: ViewActions): ViewHost {
  const who = h("div");
  const title = h("div", { class: "title one-line", text: "Mientras no estabas" });
  const rows = h("div", { class: "res" });
  const row = h("div", { class: "actions" },
    btn("OK", "primary", () => {
      State.away = null;
      actions.collapse();
    }),
  );
  const body = stack(116, 16, who, title, rows, row);
  body.classList.add("list");
  const el = h("div", { class: "view" }, card("indigo", body));
  let key = "";
  return {
    el,
    sync() {
      const away = State.away;
      const shown = away?.rows.slice(0, MAX_CARD_ROWS) ?? [];
      const next = `${away?.minutes}|${shown.map((r) => `${r.sessionId}:${r.status}:${r.detail}`).join("|")}`;
      if (next === key) return;
      key = next;
      clear(who);
      const more = (away?.rows.length ?? 0) - shown.length;
      who.append(h("div", { class: "who-row" },
        h("span", { class: "n", text: "¡Volviste!" }),
        h("span", { text: `fuera ${away?.minutes ?? 0} min${more > 0 ? ` · ${more} más` : ""}` }),
      ));
      clear(rows);
      for (const r of shown) {
        rows.append(h("button", {
          class: "res-row option",
          title: r.detail,
          onclick: () => {
            const approval = State.pendingApproval?.sessionId === r.sessionId;
            actions.selectSession(r.sessionId);
            if (approval) actions.setView("approval");
          },
        },
          dot(STATUS_COLORS[r.status], 7),
          h("b", { text: r.name }),
          h("span", { text: r.detail }),
        ));
      }
    },
  };
}

// ── Confused ──────────────────────────────────────────────────────────────────

function buildConfused(): ViewHost {
  const body = h(
    "div",
    { class: "stack", style: "padding:0 18px 0 128px" },
    h("div", { class: "title", text: "Demasiados golpes de una vez." }),
    h("div", { class: "sub", text: "Dame un segundo — vuelvo al trabajo en tres segundos." }),
  );
  return { el: h("div", { class: "view" }, card("pink", body)), sync() {} };
}

// ── Note ──────────────────────────────────────────────────────────────────────

function buildNote(): ViewHost {
  const title = h("div", { class: "title" });
  const el = h("div", { class: "view" }, card(null, h("div", { class: "stack", style: "padding:0 18px 0 98px" }, title)));
  return {
    el,
    sync() {
      title.textContent = State.noteMessage ?? "";
    },
  };
}

// ── In-island settings ────────────────────────────────────────────────────────

function buildSettings(actions: ViewActions): ViewHost {
  const soundSwitch = h("button", { class: "switch", onclick: () => actions.toggleSound() });
  const volume = h("input", {
    type: "range", min: "0", max: "0.2", step: "0.005",
    oninput: (e: Event) => actions.setVolume(Number((e.target as HTMLInputElement).value)),
  }) as HTMLInputElement;
  const autoLabel = h("span", {});
  const segButtons = AUTO_CLOSE_CHOICES.map((s) =>
    h("button", { onclick: () => actions.setAutoClose(s) }, `${s} s`),
  );
  const claudeBadge = h("span", { class: "status-badge" });
  const apiBadge = h("span", { class: "status-badge" });

  const rows = h(
    "div",
    { class: "settings-rows" },
    h("div", { class: "settings-row" }, soundSwitch, h("span", { text: "Sonido" }), volume),
    h(
      "div",
      { class: "settings-row" },
      svg(ICONS.timer, 12),
      autoLabel,
      h("div", { class: "seg" }, ...segButtons),
    ),
    h(
      "div",
      { class: "settings-row", style: "gap:14px" },
      claudeBadge,
      apiBadge,
      h("div", { class: "grow" }),
      h("button", {
        class: "link-btn",
        style: "color:#8e939c;font-size:11.5px",
        text: "Ajustes…",
        onclick: () => actions.openSettingsWindow(),
      }),
    ),
  );

  const el = h("div", { class: "view" },
    card(null, h("div", { class: "stack", style: "padding:14px 16px 14px 84px" }, rows)));

  return {
    el,
    sync() {
      const s = State.settings;
      soundSwitch.classList.toggle("on", s.soundEnabled);
      volume.value = String(s.soundVolume);
      volume.style.opacity = s.soundEnabled ? "1" : "0.4";
      autoLabel.textContent = `Cierre automático · ${Math.round(s.autoCloseInterval)} s`;
      segButtons.forEach((b, i) => b.classList.toggle("on", s.autoCloseInterval === AUTO_CLOSE_CHOICES[i]));
      clear(claudeBadge);
      claudeBadge.append(
        dot(s.hooksInstalled ? "#22C55E" : "#F4505E", 6),
        h("span", { text: "Claude Code" }),
      );
      clear(apiBadge);
      apiBadge.append(
        dot(s.chatEngine === "api" ? "#F5A524" : "#22C55E", 6),
        h("span", { text: s.chatEngine === "api" ? "Chat · API key" : "Chat · tu cuenta de Claude" }),
      );
    },
  };
}

// ── Sessions ──────────────────────────────────────────────────────────────────

/** Every live Claude Code session: name, what it's doing, and whether it needs you. */
function buildSessions(actions: ViewActions): ViewHost {
  const list = h("div", { class: "session-list" });
  const empty = h("div", { class: "sub", text: "No hay sesiones de Claude Code abiertas." });
  const body = h("div", { class: "sessions-body" }, list);
  const el = h("div", { class: "view" }, card(null, body));
  let key = "";

  function row(id: string): HTMLElement | null {
    const s = State.sessions.find((x) => x.id === id);
    if (!s) return null;
    const color = STATUS_COLORS[s.status];
    const resets = s.status === "ratelimit" ? rateCountdown(s) : "";
    const status = isWaiting(s)
      ? `${STATUS_LABELS[s.status]} · ${waitingSinceLabel(s)}`
      : resets
        ? `${STATUS_LABELS[s.status]} · ${resets}`
        : STATUS_LABELS[s.status];
    const ended = s.status === "finished" || s.status === "error";
    const queued = s.queue.length > 0 ? ` · ${s.queue.length} en cola` : "";
    const detail = (ended && summaryText(s.summary)) || (s.steps.at(-1) ?? "");
    const r = h(
      "button",
      {
        class: s.id === State.currentSessionId ? "session-row on" : "session-row",
        title: s.cwd,
        onclick: () => actions.selectSession(s.id),
      },
      dot(color, 7),
      h("span", { class: "session-name", text: s.name }),
      h("span", { class: "session-status", style: `color:${color}`, text: status + queued }),
      h("span", { class: "session-detail", text: detail }),
    );
    if (isWaiting(s)) r.classList.add("waiting");
    return r;
  }

  return {
    el,
    sync() {
      const next = State.sessions
        .map((s) => [s.id, s.name, s.status, s.steps.at(-1), s.waitingSince, rateCountdown(s),
          summaryText(s.summary), s.id === State.currentSessionId, s.queue.length].join("~"))
        .join("|");
      if (next === key) return;
      key = next;
      clear(list);
      if (State.sessions.length === 0) {
        list.append(empty);
        return;
      }
      for (const s of State.sessions) {
        const r = row(s.id);
        if (r) list.append(r);
      }
    },
  };
}

// ── Prompt queue ──────────────────────────────────────────────────────────────

/**
 * What to tell the current session next. Each prompt waits for the turn to end,
 * then the Stop hook hands it to Claude, which carries on without you typing.
 */
function buildQueue(actions: ViewActions): ViewHost {
  const who = h("div", { class: "who" });
  const hint = h("div", { class: "sub queue-hint" });
  const list = h("div", { class: "queue-list" });
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Lo siguiente que quieres pedirle…",
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Poner en cola" }, svg(ICONS.plus, 11));
  const bar = h("div", { class: "chat-bar" }, input, send);
  const el = h(
    "div",
    { class: "view" },
    h("div", { class: "card wash chat-card" }, h("div", { class: "chat-body queue-body" }, who, hint, list, bar)),
  );
  (el.querySelector(".card") as HTMLElement).style.setProperty("--wash", "rgba(99,102,241,0.5)");
  let key = "";

  function add() {
    const s = State.currentSession;
    if (!s || !queuePrompt(s, input.value)) return;
    input.value = "";
    actions.blip();
    State.notify();
  }

  send.addEventListener("click", add);
  input.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") {
      e.preventDefault();
      add();
    }
    e.stopPropagation(); // Escape closes the island, not the field
  });

  return {
    el,
    sync() {
      const s = State.currentSession;
      const full = !!s && s.queue.length >= MAX_QUEUE;
      input.disabled = !s || full;
      input.placeholder = !s
        ? "No hay sesión de Claude Code abierta."
        : full ? "La cola está llena." : "Lo siguiente que quieres pedirle…";
      const next = s ? [s.id, s.name, s.status, ...s.queue].join("~") : "";
      if (next === key) return;
      key = next;
      clear(who);
      clear(list);
      if (!s) {
        hint.textContent = "";
        return;
      }
      who.append(
        dot(STATUS_COLORS[s.status], 7),
        h("span", { class: "name", text: s.name }),
        h("span", { class: "tool", text: "Cola" }),
      );
      hint.textContent = isBusy(s)
        ? "Se envían en orden, cada vez que Claude termine un turno."
        : "Se envía cuando Claude termine el próximo turno.";
      s.queue.forEach((prompt, i) => {
        list.append(h(
          "div",
          { class: "queue-row" },
          h("span", { class: "queue-n", text: String(i + 1) }),
          h("span", { class: "queue-text", text: prompt, title: prompt }),
          h(
            "button",
            {
              class: "icon-btn queue-x",
              title: "Quitar de la cola",
              onclick: () => {
                unqueuePrompt(s, i);
                State.notify();
              },
            },
            svg(ICONS.xmark, 8),
          ),
        ));
      });
    },
    focus() {
      input.focus();
    },
  };
}

// ── Placeholders filled in later stages ───────────────────────────────────────

function buildPlaceholder(title: string, sub: string): ViewHost {
  const body = h(
    "div",
    { class: "stack", style: "padding:0 18px 0 118px" },
    h("div", { class: "title", text: title }),
    h("div", { class: "sub", text: sub }),
  );
  return { el: h("div", { class: "view" }, card(null, body)), sync() {} };
}

// ── Registry ──────────────────────────────────────────────────────────────────

export function buildViews(
  actions: ViewActions,
  onChatHeightChange: () => void,
): Map<IslandViewName, ViewHost> {
  const map = new Map<IslandViewName, ViewHost>();
  map.set("overview", buildOverview(actions));
  map.set("empty", buildEmpty(actions));
  map.set("approval", buildApproval(actions));
  map.set("question", buildQuestion(actions));
  map.set("error", buildError(actions));
  map.set("ratelimit", buildRateLimit(actions));
  map.set("finished", buildFinished(actions));
  map.set("confused", buildConfused());
  map.set("note", buildNote());
  map.set("settings", buildSettings(actions));
  map.set("sessions", buildSessions(actions));
  map.set("queue", buildQueue(actions));
  map.set("prompt", buildPrompt(onChatHeightChange));
  map.set("library", buildLibrary(actions));
  map.set("away", buildAway(actions));
  map.set("upload", buildUpload());
  map.set("uploading", buildUploading());
  map.set("choose", buildChoose(actions));
  // Not in the Windows v1: sending a file by email, window attach + web result.
  map.set("mail", buildPlaceholder("Enviar por correo no está en esta versión.", ""));
  map.set("searching", buildPlaceholder("Claude está buscando…", ""));
  map.set("result", buildPlaceholder("Resultado", ""));
  return map;
}
