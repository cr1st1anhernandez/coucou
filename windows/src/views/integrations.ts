// Integration cards shown in the overview's left card — DOM ports of
// IntegrationCardView and friends from IslandViewContent.swift.

import { h, svg, clear, dot } from "./dom";
import { ICONS } from "./icons";
import { State, type AgentTask } from "../core/state";
import { Bridge } from "../core/bridge";

function header(color: string, name: string, kind: string, extra?: Node): HTMLElement {
  const row = h("div", { class: "int-head" }, dot(color, 7), h("b", { text: name }), h("span", { text: kind }));
  if (extra) row.append(extra);
  return row;
}

/** Highlighted first row + plain rows, the layout every list card shares. */
function listRow(accent: string, first: boolean, ...children: Node[]): HTMLElement {
  const row = h("div", { class: first ? "int-row first" : "int-row" }, dot(accent, 5), ...children);
  if (first) row.style.background = `${accent}14`;
  return row;
}

function get(id: string): Record<string, unknown> {
  return (State.integrations[id]?.data ?? {}) as Record<string, unknown>;
}

function arr(id: string, key: string): Record<string, unknown>[] {
  const v = get(id)[key];
  return Array.isArray(v) ? (v as Record<string, unknown>[]) : [];
}

// ── Not configured / idle ─────────────────────────────────────────────────────

const OPEN_URLS: Record<string, string> = {
  integration_github: "https://github.com",
};

function idleCard(task: AgentTask, openSettings: () => void): HTMLElement {
  const info = State.integrations[task.id];
  const configured = info?.configured ?? false;
  const error = info?.error ?? null;
  // The Claude Code pill is about hooks, not a key — the macOS wording would be
  // misleading here.
  const missing = task.id === "integration_claude" ? "Hooks no instalados" : "Clave no configurada";
  const label = error ?? (configured ? "Conectado · cargando…" : missing);
  const statusColor = error || !configured ? "#F4505E" : "#22C55E";

  const actions = h("div", { class: "int-actions" });
  if (task.id === "integration_claude") {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}b3`,
        text: "Abrir Warp",
        onclick: () => void Bridge.openTerminal(task.sessionCwd ?? null, task.sessionFocusUrl ?? null),
      }),
    );
  } else if (OPEN_URLS[task.id]) {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: `Abrir ${task.name}`,
        onclick: () => void Bridge.openUrl(OPEN_URLS[task.id]),
      }),
    );
  }
  if (configured) {
    actions.append(
      h("button", {
        class: "link-btn",
        style: `color:${task.color}d9`,
        text: "Actualizar",
        onclick: () => void Bridge.refreshIntegration(task.id),
      }),
    );
  } else {
    actions.append(
      h("button", { class: "link-btn", style: "color:#8e939c", text: "Ajustes…", onclick: openSettings }),
    );
  }

  return h(
    "div",
    { class: "int-card" },
    header(task.color, task.name, "Integración"),
    h("div", { class: "int-status" }, dot(statusColor, 5), h("span", { text: label })),
    actions,
  );
}

// ── GitHub ────────────────────────────────────────────────────────────────────

function statRow(icon: string, color: string, label: string, value: string): HTMLElement {
  return h(
    "div",
    { class: "int-stat" },
    h("i", { class: "int-stat-icon", style: `color:${color}` }, svg(icon, 10)),
    h("span", { class: "int-stat-label", text: label }),
    h("span", { class: "int-stat-value", text: value }),
  );
}

/** Accent and label per PR status, worst news first (see pr_status in Rust). */
const PR_STATUS: Record<string, [string, string]> = {
  ci_failed: ["#F4505E", "CI falló"],
  changes: ["#F5A524", "Cambios pedidos"],
  ci_running: ["#3B9EFF", "CI corriendo"],
  approved: ["#22C55E", "Aprobado"],
  draft: ["#6B7079", "Borrador"],
  review: ["#8E939C", "En review"],
};

/** Your open PRs, the one that changed last on top; a click opens it. */
function githubPrsCard(prs: Record<string, unknown>[]): HTMLElement {
  const rows = h("div", { class: "int-rows" });
  prs.slice(0, 3).forEach((pr, i) => {
    const [accent, label] = PR_STATUS[String(pr.status)] ?? PR_STATUS.review;
    const row = listRow(
      accent,
      i === 0,
      h("span", { class: "int-name", text: String(pr.title ?? "") }),
      h("span", { class: "int-ago", style: `color:${accent}`, text: label }),
    );
    row.classList.add("link");
    row.title = `${pr.repo ?? ""}#${pr.number ?? ""}`;
    row.addEventListener("click", () => void Bridge.openUrl(String(pr.url ?? "")));
    rows.append(row);
  });
  return h("div", { class: "int-card" }, header("#F4505E", "GitHub", "Tus PRs"), rows);
}

function githubCard(): HTMLElement {
  const prs = arr("integration_github", "prs");
  if (prs.length > 0) return githubPrsCard(prs);
  const d = get("integration_github");
  const stars = Number(d.totalStars ?? 0);
  const repos = Number(d.totalRepos ?? 0);
  const fmt = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));
  return h(
    "div",
    { class: "int-card" },
    header("#F4505E", "GitHub", "Resumen"),
    h(
      "div",
      { class: "int-stats" },
      statRow(ICONS.star, "#F5A524", "Estrellas", fmt(stars)),
      statRow(ICONS.stack, "#6B7079", "Repositorios", String(repos)),
    ),
  );
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

export interface IntegrationCardHooks {
  detailOpen: boolean;
  openDetail(): void;
  closeDetail(): void;
  openSettings(): void;
}

/** True when this integration has data worth showing instead of the idle card. */
export function hasIntegrationData(id: string): boolean {
  const info = State.integrations[id];
  if (!info || info.error) return false;
  switch (id) {
    case "integration_github":
      return get(id).totalRepos != null;
    default:
      return false;
  }
}

export function renderIntegrationCard(task: AgentTask, hooks: IntegrationCardHooks): HTMLElement {
  if (!hasIntegrationData(task.id)) return idleCard(task, hooks.openSettings);

  switch (task.id) {
    case "integration_github":
      return githubCard();
    default:
      return idleCard(task, hooks.openSettings);
  }
}

export { clear };
