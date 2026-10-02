// The library — <Documents>\mochi, one tab per project, its prompts, scripts
// and notes side by side, and a chat that files new things away.
//
// The buttons only ever copy: a prompt's text, a script or note as it is, or
// an `@"path"` Claude Code reads by itself. "A Warp" and "Pegar" also bring
// Warp to the front (and paste, for the second) — nothing presses Enter, and
// scripts are never run from here. Which of copy / A Warp / Pegar show up is
// up to the user (Ajustes → Biblioteca), with at least one always on.
//
// Saving goes through the library's own chat: Claude Code, working inside the
// library folder only, decides the project, the kind and the title.

import { h, svg, clear, dot } from "./dom";
import { ICONS } from "./icons";
import { buildDropZone } from "./dropzone";
import {
  Bridge, onEvent,
  type CodeChatEvent, type LibraryData, type LibraryItem, type LibraryKind, type LibraryMode, type LibraryProject,
} from "../core/bridge";
import { colorForProject } from "../core/layout";
import { Sound } from "../core/sound";
import { State } from "../core/state";
import type { ViewActions, ViewHost } from "./views";

const COLUMNS: { kind: LibraryKind; label: string; icon: string; filled: boolean }[] = [
  { kind: "prompt", label: "Prompts", icon: ICONS.bubble, filled: true },
  { kind: "script", label: "Scripts", icon: ICONS.terminal, filled: false },
  { kind: "note", label: "Notas", icon: ICONS.doc, filled: true },
];

const SORT_PROMPT =
  "Ordena la biblioteca: revisa todos los proyectos, pon títulos claros, etiquetas útiles y el orden (los más usados primero), " +
  "junta duplicados y mueve a su proyecto lo que esté en el lugar equivocado. No borres nada sin necesidad. " +
  "Al final dime en una línea qué cambiaste.";

const MODE_BUTTONS: { mode: LibraryMode; key: "copy" | "warp" | "paste"; label: string; icon: string; done: string }[] = [
  { mode: "copy", key: "copy", label: "Copiar", icon: ICONS.copy, done: "Copiado · pégalo con Ctrl+V" },
  { mode: "warp", key: "warp", label: "A Warp", icon: ICONS.terminal, done: "Copiado y Warp al frente · pega con Ctrl+V" },
  { mode: "paste", key: "paste", label: "Pegar en Warp", icon: ICONS.paste, done: "Pegado en Warp · el Enter lo das tú" },
];

/** The dev preview (dev/island-preview.ts) shows made-up projects outside Coucou. */
let previewData: LibraryData | null = null;
export function setLibraryPreview(data: LibraryData) {
  previewData = data;
}

function iconBtn(path: string, title: string, onClick: () => void, filled = false): HTMLElement {
  return h(
    "button",
    {
      class: "lib-act",
      title,
      onclick: (e: Event) => {
        e.stopPropagation();
        onClick();
      },
    },
    filled ? svg(path, 12) : svg(path, 12, { stroke: 2 }),
  );
}

export function buildLibrary(actions: ViewActions): ViewHost {
  let data: LibraryData | null = null;
  let current: string | null = null;
  let loading = false;
  let busy = false;
  /** One line under the lists: what just happened, or Claude's reply. */
  let status = "";
  let statusKind: "info" | "ok" | "err" = "info";
  let streamed = "";
  let renderedKey = "";

  const tabs = h("div", { class: "lib-tabs" });
  const folderBtn = h(
    "button",
    { class: "lib-top-btn", title: "Abrir la carpeta Documentos\\mochi", onclick: () => void Bridge.libraryOpenFolder() },
    svg(ICONS.folder, 12, { stroke: 2 }),
  );
  const sortBtn = h(
    "button",
    { class: "lib-top-btn sort", title: "Claude Code ordena títulos, etiquetas y duplicados", onclick: () => void ask(SORT_PROMPT, true) },
    svg(ICONS.sparkles, 12),
    h("span", { text: "Ordenar con Claude" }),
  );
  const cols = h("div", { class: "lib-cols" });
  const statusLine = h("div", { class: "lib-status" });
  const drop = buildDropZone("library", [".md", ".ps1", ".sh", ".txt"]);
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Guarda algo: \"guarda este prompt en coucou: …\"",
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Guardar con Claude" }, svg(ICONS.arrowUp, 11));
  const bar = h("div", { class: "chat-bar lib-bar" }, input, send);

  const body = h(
    "div",
    { class: "lib-body" },
    h("div", { class: "lib-top" }, h("div", { class: "lib-bot-spot" }), tabs, folderBtn, sortBtn),
    cols,
    statusLine,
    h("div", { class: "lib-entry" }, drop.el, bar),
  );
  const el = h("div", { class: "view" }, h("div", { class: "card lib-card" }, body));

  void onEvent<CodeChatEvent>("code-chat", (e) => {
    if (e.channel !== "library" || !busy) return;
    if (e.kind === "delta") {
      streamed += e.text;
      say(streamed.split("\n").filter(Boolean).at(-1) ?? "", "info");
    } else if (!streamed) {
      say(e.text, "info");
    }
  });

  function say(text: string, kind: "info" | "ok" | "err" = "info") {
    status = text;
    statusKind = kind;
    State.notify();
  }

  async function reload() {
    if (loading) return;
    loading = true;
    try {
      data = previewData ?? (await Bridge.libraryList());
      if (!data.projects.some((p) => p.id === current)) current = data.projects[0]?.id ?? null;
    } catch (err) {
      say(String(err).replace(/^Error:\s*/, ""), "err");
    } finally {
      loading = false;
      renderedKey = "";
      State.notify();
    }
  }

  async function ask(text: string, sorting = false) {
    const file = State.attachments.library;
    const prompt = text.trim() || (file ? "Guarda este archivo en la biblioteca." : "");
    if (!prompt || busy) return;
    busy = true;
    streamed = "";
    input.value = "";
    State.attachments.library = null;
    Sound.play("send", "chat");
    say(sorting ? "Claude está ordenando la biblioteca…" : "Claude lo está acomodando…");
    State.stateOverride = "thinking";
    State.notify();
    try {
      const reply = await Bridge.codeChat("library", prompt, file ? [file.path] : []);
      const line = reply.text.split("\n").map((l) => l.trim()).filter(Boolean).join(" ");
      say(line || "Listo.", "ok");
      Sound.play("finish", "chat");
      actions.emote("happy");
      // Show the project Claude just saved into.
      const m = line.match(/Guardado en\s+([^>›]+?)\s*[>›]/i);
      await reload();
      if (m && data) {
        const name = m[1].trim().toLowerCase();
        const hit = data.projects.find((p) => p.id.toLowerCase() === name || p.name.toLowerCase() === name);
        if (hit) current = hit.id;
      }
    } catch (err) {
      say(String(err).replace(/^Error:\s*/, ""), "err");
      Sound.play("error", "chat");
    } finally {
      busy = false;
      State.stateOverride = null;
      renderedKey = "";
      State.notify();
    }
  }

  async function use(item: LibraryItem, project: LibraryProject, mode: LibraryMode, done: string) {
    try {
      await Bridge.libraryUse(item.path, item.kind, mode, project.repo);
      Sound.play("approve");
      actions.emote("wink");
      say(`${item.title} · ${done}`, "ok");
    } catch (err) {
      Sound.play("error", "chat");
      say(String(err).replace(/^Error:\s*/, ""), "err");
    }
  }

  function itemRow(item: LibraryItem, project: LibraryProject): HTMLElement {
    const acts = h("div", { class: "lib-acts" });
    if (item.kind === "note") {
      acts.append(
        iconBtn(ICONS.copy, "Copiar el contenido", () => void use(item, project, "copy", "Copiado")),
        iconBtn(ICONS.at, "Copiar @ruta para Claude Code", () => void use(item, project, "ref", "@ruta copiada para Claude Code")),
      );
    } else {
      const modes = State.settings.pasteModes;
      for (const b of MODE_BUTTONS) {
        if (modes[b.key]) acts.append(iconBtn(b.icon, b.label, () => void use(item, project, b.mode, b.done)));
      }
      if (item.kind === "script") {
        acts.append(iconBtn(ICONS.at, "Copiar @ruta para Claude Code", () => void use(item, project, "ref", "@ruta copiada para Claude Code")));
      }
    }
    return h(
      "div",
      { class: "lib-item", title: item.preview || item.file },
      h("div", { class: "lib-item-text" },
        h("b", { text: item.title }),
        h("span", { text: item.preview || item.file }),
      ),
      acts,
    );
  }

  function render() {
    clear(tabs);
    clear(cols);
    const projects = data?.projects ?? [];
    for (const p of projects) {
      tabs.append(h(
        "button",
        {
          class: p.id === current ? "lib-tab on" : "lib-tab",
          title: p.repo ?? p.name,
          onclick: () => {
            current = p.id;
            renderedKey = "";
            actions.blip();
            State.notify();
          },
        },
        dot(p.color ?? colorForProject(p.name), 6),
        h("span", { text: p.name }),
      ));
    }
    const project = projects.find((p) => p.id === current);
    if (!project) {
      cols.classList.add("empty");
      cols.append(h(
        "div",
        { class: "lib-empty" },
        h("b", { text: loading ? "Cargando la biblioteca…" : "Tu biblioteca está vacía." }),
        h("span", {
          text: "Escríbele abajo, por ejemplo: guarda este prompt en coucou: revisa mis PRs y dime qué falta. " +
            "O suelta un .md o un script. Todo vive en Documentos\\mochi.",
        }),
      ));
      return;
    }
    cols.classList.remove("empty");
    for (const c of COLUMNS) {
      const items = project.items.filter((i) => i.kind === c.kind);
      const list = h("div", { class: "lib-list" });
      for (const it of items) list.append(itemRow(it, project));
      if (items.length === 0) list.append(h("div", { class: "lib-none", text: "Nada todavía" }));
      cols.append(h(
        "div",
        { class: "lib-col" },
        h("div", { class: "lib-col-head" },
          c.filled ? svg(c.icon, 11) : svg(c.icon, 11, { stroke: 2 }),
          h("span", { text: c.label }),
          h("i", { text: String(items.length) }),
        ),
        list,
      ));
    }
  }

  send.addEventListener("click", () => void ask(input.value));
  input.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") {
      e.preventDefault();
      void ask(input.value);
    }
    e.stopPropagation();
  });

  return {
    el,
    sync() {
      drop.sync();
      const modes = State.settings.pasteModes;
      const key = [
        current, loading, JSON.stringify(data?.projects.map((p) => [p.id, p.items.length, p.items.map((i) => i.path + i.title)])),
        modes.copy, modes.warp, modes.paste,
      ].join("~");
      if (key !== renderedKey) {
        renderedKey = key;
        render();
      }
      statusLine.textContent = status;
      statusLine.className = `lib-status ${statusKind}`;
      sortBtn.classList.toggle("off", busy);
      input.disabled = busy;
    },
    /** The library just opened. */
    focus() {
      // Re-read the folder every time: Claude Code or the user may have changed
      // it since. Nothing watches it in the background.
      void reload();
      input.focus();
    },
  };
}
