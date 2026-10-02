// Settings window — the place where anything that writes to disk is confirmed.
// Stage 2 covers the Claude Code hooks and the general preferences; API keys and
// integrations land here too in a later stage.

import "./settings.css";
import { Bridge, onEvent, type HookStatus } from "../core/bridge";
import { DEFAULT_SETTINGS, type Settings } from "../core/state";
import { SOUND_CUES, Sound, cueEnabled } from "../core/sound";
import { h, clear } from "../views/dom";

let settings: Settings = { ...DEFAULT_SETTINGS };
let version = "";

const root = document.getElementById("settings-root")!;

async function save() {
  await Bridge.saveSettings(settings);
}

// ── Reusable bits ─────────────────────────────────────────────────────────────

function toggle(on: boolean, onChange: (v: boolean) => void): HTMLElement {
  const el = h("button", { class: on ? "switch on" : "switch", "aria-pressed": on });
  el.addEventListener("click", () => {
    const next = !el.classList.contains("on");
    el.classList.toggle("on", next);
    onChange(next);
  });
  return el;
}

function statusDot(ok: boolean): HTMLElement {
  return h("i", { class: "dot", style: `background:${ok ? "#22c55e" : "#f4505e"}` });
}

function renderDiff(text: string): HTMLElement {
  const box = h("div", { class: "diff" });
  for (const line of text.split("\n")) {
    const cls = line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : "ctx";
    box.append(h("div", { class: cls, text: line }));
  }
  return box;
}

// ── Claude Code section ───────────────────────────────────────────────────────

function claudeSection(status: HookStatus): HTMLElement {
  const body = h("div", { style: "display:flex;flex-direction:column;gap:12px" });
  const section = h(
    "section",
    {},
    h("h2", {}, statusDot(status.installed), h("span", { text: "Claude Code" })),
    body,
  );

  const rebuild = async () => {
    const fresh = await Bridge.hooksStatus();
    if (fresh) Object.assign(status, fresh);
    clear(body);
    draw();
    const head = section.querySelector("h2")!;
    clear(head);
    head.append(statusDot(status.installed), h("span", { text: "Claude Code" }));
  };

  function draw() {
    body.append(
      h("div", {
        class: "hint",
        text: status.installed
          ? "Coucou está conectado a tus sesiones de Claude Code. Las herramientas, preguntas y solicitudes de permiso aparecen en la isla, y puedes responderlas ahí."
          : "Instala los hooks para ver tus sesiones de Claude Code en la isla y aprobar permisos sin dejar lo que estás haciendo.",
      }),
      h("div", { class: "row" },
        h("label", { text: "settings.json" }),
        h("span", { class: "path", text: status.settingsPath }),
      ),
      h("div", { class: "row" },
        h("label", { text: "Relay" }),
        h("span", { class: "path", text: status.hookPath }),
        statusDot(status.hookReady),
      ),
    );

    if (!status.hookReady) {
      body.append(h("div", {
        class: "notice warn",
        text: "coucou-hook.exe todavía no está en su lugar. Reinicia Coucou; si sigue fallando, compílalo con `cargo build -p coucou-hook`.",
      }));
    }

    const actions = h("div", { class: "row" });
    const install = h("button", {
      class: "primary",
      text: status.installed ? "Reinstalar hooks…" : "Instalar hooks…",
      onclick: () => showPreview(true),
    });
    // Writing hook commands that point at a relay which isn't there would give
    // every Claude Code session a broken hook and nothing to show for it.
    if (!status.hookReady) {
      install.disabled = true;
      install.title = "El relay todavía no está instalado.";
    }
    actions.append(install);
    if (status.installed) {
      actions.append(h("button", {
        class: "danger",
        text: "Desinstalar hooks…",
        onclick: () => showPreview(false),
      }));
    }
    body.append(actions);
  }

  async function showPreview(install: boolean) {
    let preview;
    try {
      preview = await Bridge.hooksPreview(install);
    } catch (err) {
      // An unreadable or invalid settings.json stops here rather than being
      // treated as empty and written over.
      clear(body);
      body.append(
        h("div", { class: "notice err", text: String(err).replace(/^Error:\s*/, "") }),
        h("div", { class: "row" }, h("button", {
          text: "Regresar",
          onclick: () => { clear(body); draw(); },
        })),
      );
      return;
    }
    if (!preview) return;
    clear(body);
    body.append(
      h("div", {
        class: "hint",
        text: install
          ? "Esto es exactamente lo que va a cambiar en tu settings.json. Tus propios hooks no se tocan."
          : "Esto solo quita las entradas de Coucou. Tus propios hooks no se tocan.",
      }),
      renderDiff(preview.diff),
      h("div", { class: "row" },
        h("span", { class: "path", text: `Respaldo → ${preview.backup}` }),
      ),
    );
    const confirm = h("button", {
      class: install ? "primary" : "danger",
      text: install ? "Respaldar y escribir" : "Respaldar y quitar",
    });
    confirm.addEventListener("click", async () => {
      confirm.disabled = true;
      try {
        const backup = await Bridge.hooksApply(install, preview.fingerprint);
        clear(body);
        body.append(h("div", {
          class: "notice ok",
          text: `Listo. Los ajustes anteriores se guardaron en ${backup}. Abre una nueva sesión de Claude Code para que tome los hooks.`,
        }));
        window.setTimeout(() => void rebuild(), 2600);
      } catch (err) {
        confirm.disabled = false;
        body.append(h("div", { class: "notice err", text: `No se pudo escribir: ${String(err)}` }));
      }
    });
    body.append(h("div", { class: "row" }, confirm, h("button", {
      text: "Cancelar",
      onclick: () => { clear(body); draw(); },
    })));
  }

  draw();
  return section;
}

// ── Claude API section ────────────────────────────────────────────────────────

const MODELS: [string, string][] = [
  ["claude-opus-5", "Claude Opus 5"],
  ["claude-sonnet-5", "Claude Sonnet 5"],
  ["claude-haiku-4-5", "Claude Haiku 4.5"],
];

function apiSection(hasKey: boolean): HTMLElement {
  const dot = statusDot(hasKey);
  const state = h("span", { class: "hint", text: hasKey ? "Clave guardada en el Administrador de credenciales de Windows." : "Todavía no hay clave — el chat necesita una." });

  const field = h("input", {
    type: "password",
    placeholder: hasKey ? "••••••••••••  (guardada)" : "sk-ant-...",
    style: "flex:1 1 auto;min-width:0",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;

  const saveBtn = h("button", { class: "primary", text: "Guardar clave" });
  const clearBtn = h("button", { class: "danger", text: "Quitar" });
  const feedback = h("div", {});

  async function refresh() {
    const present = (await Bridge.secretPresent("anthropic-api-key")) ?? false;
    dot.style.background = present ? "#22c55e" : "#f4505e";
    state.textContent = present
      ? "Clave guardada en el Administrador de credenciales de Windows."
      : "Todavía no hay clave — el chat necesita una.";
    field.placeholder = present ? "••••••••••••  (guardada)" : "sk-ant-...";
    clearBtn.style.display = present ? "" : "none";
  }

  saveBtn.addEventListener("click", async () => {
    const value = field.value.trim();
    if (!value) return;
    clear(feedback);
    try {
      await Bridge.secretSet("anthropic-api-key", value);
      field.value = "";
      feedback.append(h("div", { class: "notice ok", text: "Guardada. Nunca toca el disco." }));
      await refresh();
    } catch (err) {
      feedback.append(h("div", { class: "notice err", text: `No se pudo guardar: ${String(err)}` }));
    }
  });

  clearBtn.addEventListener("click", async () => {
    clear(feedback);
    try {
      await Bridge.secretClear("anthropic-api-key");
      feedback.append(h("div", { class: "notice ok", text: "Clave eliminada." }));
      await refresh();
    } catch (err) {
      feedback.append(h("div", { class: "notice err", text: `No se pudo quitar: ${String(err)}` }));
    }
  });

  const model = h("select", {}) as HTMLSelectElement;
  for (const [id, label] of MODELS) model.append(h("option", { value: id, text: label }));
  if (!MODELS.some(([id]) => id === settings.model)) {
    model.append(h("option", { value: settings.model, text: settings.model }));
  }
  model.value = settings.model;
  model.addEventListener("change", () => {
    settings.model = model.value;
    void save();
  });

  clearBtn.style.display = hasKey ? "" : "none";

  const engine = h("select", {}) as HTMLSelectElement;
  engine.append(
    h("option", { value: "claudeCode", text: "Mi cuenta de Claude (vía Claude Code)" }),
    h("option", { value: "api", text: "API key" }),
  );
  engine.value = settings.chatEngine;
  const apiRows = [
    h("div", { class: "row" }, h("label", { text: "API key" }), field, saveBtn, clearBtn),
    h("div", { class: "row" }, h("label", { text: "Modelo" }), model),
  ];
  const engineHint = h("span", { class: "hint" });
  function showEngine() {
    const api = settings.chatEngine === "api";
    for (const r of apiRows) r.style.display = api ? "" : "none";
    state.style.display = api ? "" : "none";
    dot.style.display = api ? "" : "none";
    engineHint.textContent = api
      ? "El chat usa tu API key y el modelo de abajo."
      : "El chat usa el Claude Code instalado, con tu cuenta (Pro, Max…) y sus conectores. Solo lee: no cambia archivos ni envía nada.";
  }
  engine.addEventListener("change", () => {
    settings.chatEngine = engine.value as Settings["chatEngine"];
    showEngine();
    void save();
  });
  showEngine();

  return h(
    "section",
    {},
    h("h2", {}, dot, h("span", { text: "Claude" })),
    h("div", { class: "row" }, h("label", { text: "Motor del chat" }), engine),
    engineHint,
    state,
    ...apiRows,
    feedback,
  );
}

// ── Library section ───────────────────────────────────────────────────────────

/**
 * How the library's buttons take a prompt to the terminal. Any mix of the
 * three, but never none: the last one on can't be switched off.
 */
function librarySection(): HTMLElement {
  const MODES: { key: keyof Settings["pasteModes"]; label: string; hint: string }[] = [
    { key: "copy", label: "Copiar", hint: "Lo deja en el portapapeles." },
    { key: "warp", label: "A Warp", hint: "Copia y trae Warp al frente; tú pegas." },
    { key: "paste", label: "Pegar en Warp", hint: "Copia, trae Warp y pega por ti. Nunca presiona Enter." },
  ];
  const note = h("div", { class: "hint" });
  const switches: HTMLElement[] = [];
  function refresh() {
    const on = MODES.filter((m) => settings.pasteModes[m.key]);
    MODES.forEach((m, i) => {
      // The last one standing is locked on.
      const locked = on.length === 1 && settings.pasteModes[m.key];
      switches[i].classList.toggle("locked", locked);
      switches[i].title = locked ? "Tiene que quedar al menos una activa" : "";
    });
    note.textContent = "Botones que aparecen en cada prompt y script de la biblioteca. Al menos uno queda activo.";
  }
  const rows = MODES.map((m) => {
    const sw = h("button", { class: settings.pasteModes[m.key] ? "switch on" : "switch" });
    sw.addEventListener("click", () => {
      const next = !settings.pasteModes[m.key];
      const others = MODES.filter((x) => x.key !== m.key && settings.pasteModes[x.key]).length;
      if (!next && others === 0) return;
      settings.pasteModes = { ...settings.pasteModes, [m.key]: next };
      sw.classList.toggle("on", next);
      refresh();
      void save();
    });
    switches.push(sw);
    return h("div", { class: "row" }, h("label", { text: m.label }), sw, h("span", { class: "hint", text: m.hint }));
  });
  refresh();
  return h(
    "section",
    {},
    h("h2", {}, h("span", { text: "Biblioteca" })),
    h("div", { class: "hint", text: "Tus prompts, scripts y notas viven en la carpeta Documentos\\mochi de este usuario." }),
    h("div", { class: "row" },
      h("label", { text: "Carpeta" }),
      h("button", { text: "Abrir Documentos\\mochi", onclick: () => void Bridge.libraryOpenFolder() }),
    ),
    note,
    ...rows,
  );
}

// ── Integrations section ──────────────────────────────────────────────────────

interface IntegrationDef {
  id: string;
  name: string;
  color: string;
  /** Credential Manager keys, in the order they are shown. */
  fields: { key: string; label: string; placeholder: string; secret: boolean }[];
}

const INTEGRATIONS: IntegrationDef[] = [
  { id: "integration_github", name: "GitHub", color: "#F4505E",
    // Classic token with `repo`, or fine-grained with Pull requests + Commit statuses (read).
    fields: [{ key: "github-token", label: "Token", placeholder: "ghp_…  (permiso repo)", secret: true }] },
];

const MAX_ACTIVE = 4;

function integrationsSection(present: Record<string, boolean>): HTMLElement {
  const note = h("div", { class: "hint" });
  const list = h("div", { style: "display:flex;flex-direction:column;gap:14px" });

  function updateNote() {
    const used = settings.activeIntegrations.length;
    note.textContent = `Elige hasta ${MAX_ACTIVE} píldoras para mostrar junto a Mochi — ${used}/${MAX_ACTIVE} en uso. Las claves se guardan en el Administrador de credenciales de Windows, nunca en disco.`;
  }

  for (const def of INTEGRATIONS) {
    const active = settings.activeIntegrations.includes(def.id);
    const sw = h("button", { class: active ? "switch on" : "switch" });
    sw.addEventListener("click", () => {
      const on = settings.activeIntegrations.includes(def.id);
      if (on) {
        settings.activeIntegrations = settings.activeIntegrations.filter((x) => x !== def.id);
      } else {
        if (settings.activeIntegrations.length >= MAX_ACTIVE) return;
        settings.activeIntegrations = [...settings.activeIntegrations, def.id];
      }
      sw.classList.toggle("on", !on);
      updateNote();
      void save();
    });

    const rows = h("div", { style: "display:flex;flex-direction:column;gap:6px;flex:1 1 auto;min-width:0" });
    for (const field of def.fields) {
      const input = h("input", {
        type: field.secret ? "password" : "text",
        placeholder: present[field.key] ? "••••••••  (guardada)" : field.placeholder,
        autocomplete: "off",
        spellcheck: "false",
        style: "flex:1 1 auto;min-width:0",
      }) as HTMLInputElement;
      const saveBtn = h("button", { text: "Guardar" });
      const dotEl = statusDot(present[field.key] ?? false);
      saveBtn.addEventListener("click", async () => {
        const value = input.value.trim();
        try {
          await Bridge.secretSet(field.key, value);
          present[field.key] = value.length > 0;
          input.value = "";
          input.placeholder = value ? "••••••••  (guardada)" : field.placeholder;
          dotEl.style.background = value ? "#22c55e" : "#f4505e";
        } catch {
          dotEl.style.background = "#f5a524";
        }
      });
      rows.append(
        h("div", { class: "row" },
          h("label", { style: "min-width:104px", text: field.label }),
          input, saveBtn, dotEl,
        ),
      );
    }

    list.append(
      h("div", { style: "display:flex;gap:12px;align-items:flex-start" },
        h("div", { style: "display:flex;align-items:center;gap:8px;min-width:132px;padding-top:4px" },
          sw,
          h("i", { class: "dot", style: `background:${def.color}` }),
          h("span", { style: "font-size:12.5px", text: def.name }),
        ),
        rows,
      ),
    );
  }

  updateNote();
  return h("section", {}, h("h2", {}, h("span", { text: "Integraciones" })), note, list);
}

// ── General section ───────────────────────────────────────────────────────────

/** Minutes a session may wait on you before Mochi nags; 0 = never. */
const WAITING_ALERT_CHOICES: [number, string][] = [
  [0, "Nunca"],
  [1, "Tras 1 min"],
  [2, "Tras 2 min"],
  [3, "Tras 3 min"],
  [5, "Tras 5 min"],
  [10, "Tras 10 min"],
];

function generalSection(): HTMLElement {
  const volume = h("input", {
    type: "range", min: "0", max: "0.2", step: "0.005",
    value: String(settings.soundVolume),
  }) as HTMLInputElement;
  volume.addEventListener("input", () => {
    settings.soundVolume = Number(volume.value);
    void save();
  });

  const autoClose = h("input", {
    type: "number", min: "5", max: "120", step: "1",
    value: String(Math.round(settings.autoCloseInterval)),
    style: "width:72px",
  }) as HTMLInputElement;
  autoClose.addEventListener("change", () => {
    settings.autoCloseInterval = Math.max(5, Math.min(120, Number(autoClose.value) || 15));
    autoClose.value = String(settings.autoCloseInterval);
    void save();
  });

  const screen = h("select", {}) as HTMLSelectElement;
  screen.append(
    h("option", { value: "primary", text: "Pantalla principal" }),
    h("option", { value: "cursor", text: "Pantalla donde está el cursor" }),
  );
  screen.value = settings.screen;
  screen.addEventListener("change", () => {
    settings.screen = screen.value as Settings["screen"];
    void save();
  });

  const waitingAlert = h("select", {}) as HTMLSelectElement;
  for (const [value, label] of WAITING_ALERT_CHOICES) {
    waitingAlert.append(h("option", { value: String(value), text: label }));
  }
  if (!WAITING_ALERT_CHOICES.some(([v]) => v === settings.waitingAlertMinutes)) {
    waitingAlert.append(h("option", {
      value: String(settings.waitingAlertMinutes),
      text: `${settings.waitingAlertMinutes} min`,
    }));
  }
  waitingAlert.value = String(settings.waitingAlertMinutes);
  waitingAlert.addEventListener("change", () => {
    settings.waitingAlertMinutes = Number(waitingAlert.value);
    void save();
  });

  return h(
    "section",
    {},
    h("h2", {}, h("span", { text: "General" })),
    h("div", { class: "row" },
      h("label", { text: "Sonido" }),
      toggle(settings.soundEnabled, (v) => { settings.soundEnabled = v; void save(); }),
      volume,
    ),
    h("div", { class: "row" },
      h("label", { text: "Cierre automático" }),
      autoClose,
      h("span", { class: "hint", text: "segundos después de salir de la isla" }),
    ),
    h("div", { class: "row" },
      h("label", { text: "Claude te espera" }),
      waitingAlert,
      h("span", { class: "hint", text: "Mochi se inquieta y suena si una sesión espera tu respuesta" }),
    ),
    h("div", { class: "row" },
      h("label", { text: "La isla vive en" }),
      screen,
    ),
    h("div", { class: "row" },
      h("label", { text: "Abrir al iniciar Windows" }),
      toggle(settings.autostart, (v) => { settings.autostart = v; void save(); }),
    ),
  );
}

// ── Sounds section ────────────────────────────────────────────────────────────

/** One switch per kind of sound, each with a ▶ to hear it first. */
function soundsSection(): HTMLElement {
  const list = h("div", { class: "sound-list" });
  for (const cue of SOUND_CUES) {
    list.append(h("div", { class: "sound-row" },
      toggle(cueEnabled(settings.soundCues, cue.id), (v) => {
        settings.soundCues = { ...settings.soundCues, [cue.id]: v };
        void save();
      }),
      h("span", { class: "sound-label", text: cue.label }),
      h("button", {
        class: "play-btn",
        title: "Escuchar",
        text: "▶",
        onclick: () => {
          Sound.setVolume(settings.soundVolume);
          void Sound.preview(cue.sample);
        },
      }),
    ));
  }
  return h(
    "section",
    {},
    h("h2", {}, h("span", { text: "Sonidos" })),
    h("div", {
      class: "hint",
      text: "Elige qué suena. El interruptor de Sonido en General los silencia todos.",
    }),
    list,
  );
}

// ── Boot ──────────────────────────────────────────────────────────────────────

async function main() {
  const boot = await Bridge.boot();
  if (boot) {
    settings = { ...settings, ...boot.settings };
    version = boot.version;
  }
  const status = (await Bridge.hooksStatus()) ?? {
    installed: false, settingsPath: "", hookPath: "", hookReady: false,
  };

  const hasKey = (await Bridge.secretPresent("anthropic-api-key")) ?? false;

  const keys = ["github-token"];
  const present: Record<string, boolean> = {};
  for (const k of keys) present[k] = (await Bridge.secretPresent(k)) ?? false;

  clear(root);
  root.append(
    h("h1", {}, h("span", { text: "Coucou" }), h("span", { class: "version", text: version })),
    claudeSection(status),
    apiSection(hasKey),
    librarySection(),
    integrationsSection(present),
    generalSection(),
    soundsSection(),
    h("div", {
      class: "hint",
      text: "Sin telemetría. Las solicitudes de red solo van a los servicios que tú configures.",
    }),
  );

  void onEvent<Settings>("settings-changed", (s) => {
    settings = { ...settings, ...s };
  });
}

void main();
