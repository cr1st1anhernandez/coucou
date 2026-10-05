// The library — <Documents>\mochi — navigated by its path, shown on top:
// Proyectos › coucou › Accesos. Each level takes the whole card: the projects,
// then a project's three categories, then one category's list.
//
// Instrucciones holds how the user wants Claude to work (prompt) and how to
// bring an environment up (entorno); Accesos, dev credentials as `clave: valor`
// lines, each value one click from the clipboard; Documentos, .docx user
// stories and templates.
//
// The buttons copy or open: an instruction's text, a value, a file (to paste
// as an attachment), or an `@"path"` Claude Code reads by itself. The trash
// asks for a second click and only ever sends the file to the Recycle Bin.
// "A Warp" and "Pegar" also bring Warp to the front (and paste, for the
// second) — nothing presses Enter, and nothing is ever run from here. Which of
// copy / A Warp / Pegar show up is up to the user (Ajustes → Biblioteca).
//
// Files move both ways: any row can be dragged out (to a terminal, a mail,
// Explorer) and files dragged in are copied into the project or category they
// are dropped on. Saving text also goes through the library's own chat, where
// Claude Code, working inside the library folder only, decides where it goes.

import { startDrag } from "@crabnebula/tauri-plugin-drag";
import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { buildAttach } from "./attach";
import {
  Bridge, onEvent,
  type CodeChatEvent, type DragDropPayload, type LibraryData, type LibraryItem, type LibraryKind, type LibraryMode,
  type LibraryProject,
} from "../core/bridge";
import { colorForProject } from "../core/layout";
import { Sound } from "../core/sound";
import { State } from "../core/state";
import type { ViewActions, ViewHost } from "./views";

interface Category {
  kind: LibraryKind;
  label: string;
  icon: string;
  filled: boolean;
  one: string;
  many: string;
  /** What the list says while a file is dragged over it. */
  drop: string;
}

const CATEGORIES: Category[] = [
  {
    kind: "instruction", label: "Instrucciones", icon: ICONS.bubble, filled: true, one: "instrucción", many: "instrucciones",
    drop: "Prompt o entorno, según el nombre",
  },
  {
    kind: "access", label: "Accesos", icon: ICONS.key, filled: false, one: "acceso", many: "accesos",
    drop: "Se leen sus líneas clave: valor",
  },
  {
    kind: "document", label: "Documentos", icon: ICONS.doc, filled: true, one: "documento", many: "documentos",
    drop: "HU o plantilla, según el nombre",
  },
];

const DOC_GROUPS: { sub: string; label: string }[] = [
  { sub: "hu", label: "Historias de usuario" },
  { sub: "plantilla", label: "Plantillas" },
  { sub: "otro", label: "Otros" },
];

type Filter = "todo" | "prompt" | "entorno";

/** Files the library reads as text; anything else is a document. */
const TEXT_EXT = /\.(md|txt|markdown|ps1|sh|bat|cmd|py|js|ts|json|env)$/i;

/** `C:\Users\dev\Projects\duo` → `duo`. */
function lastFolder(path: string): string {
  return path.split(/[\\/]+/).filter(Boolean).at(-1) ?? path;
}

const SORT_PROMPT =
  "Ordena la biblioteca: revisa todos los proyectos, pon títulos claros, etiquetas útiles, el tipo (prompt o entorno) " +
  "de cada instrucción y el orden (los más usados primero); deja cada acceso como líneas clave: valor, uno por archivo; " +
  "junta duplicados y mueve a su proyecto lo que esté en el lugar equivocado. No borres nada sin necesidad. " +
  "Al final dime en una línea qué cambiaste.";

const MODE_BUTTONS: { mode: LibraryMode; key: "copy" | "warp" | "paste"; label: string; icon: string; done: string }[] = [
  { mode: "copy", key: "copy", label: "Copiar", icon: ICONS.copy, done: "Copiado · pégalo con Ctrl+V" },
  { mode: "warp", key: "warp", label: "A Warp", icon: ICONS.terminal, done: "Copiado y Warp al frente · pega con Ctrl+V" },
  { mode: "paste", key: "paste", label: "Pegar en Warp", icon: ICONS.paste, done: "Pegado en Warp · el Enter lo das tú" },
];

const REF_DONE = "@ruta copiada para Claude Code";

/** The dev preview (dev/island-preview.ts) shows made-up projects outside Coucou. */
let previewData: LibraryData | null = null;
export function setLibraryPreview(data: LibraryData) {
  previewData = data;
}

/** A file dragged over the library while it's up; false hands it to the chat bar. */
let dropHandler: ((e: DragDropPayload) => boolean) | null = null;
export function libraryDrop(e: DragDropPayload): boolean {
  return dropHandler?.(e) ?? false;
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

/** "48 KB", "1.2 MB". */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** "2 oct". */
function formatDate(ms: number | null): string {
  if (!ms) return "";
  return new Date(ms).toLocaleDateString("es-MX", { day: "numeric", month: "short" }).replace(".", "");
}

/** The letter on a document's badge, and its colour. */
function docBadge(file: string): { letter: string; color: string } {
  const ext = file.split(".").pop()?.toLowerCase() ?? "";
  if (ext.startsWith("doc") || ext === "rtf" || ext === "odt") return { letter: "W", color: "#2b579a" };
  if (ext.startsWith("xls") || ext === "csv") return { letter: "X", color: "#217346" };
  if (ext.startsWith("ppt")) return { letter: "P", color: "#c43e1c" };
  if (ext === "pdf") return { letter: "PDF", color: "#c62828" };
  return { letter: ext.slice(0, 3).toUpperCase() || "?", color: "#4b5563" };
}

/** A connection string built from an access's fields, when it looks like a database. */
function connectionUrl(item: LibraryItem): string | null {
  const f = new Map(item.fields.map(([k, v]) => [k.toLowerCase(), v]));
  const given = f.get("url") ?? f.get("uri") ?? f.get("connection") ?? f.get("cadena");
  if (given) return given;
  const name = `${item.title} ${item.file}`.toLowerCase();
  const scheme =
    /postgres/.test(name) ? "postgresql" : /mysql|maria/.test(name) ? "mysql" : /redis/.test(name) ? "redis"
      : /mongo/.test(name) ? "mongodb" : /sql ?server|mssql/.test(name) ? "sqlserver" : null;
  const host = f.get("host") ?? f.get("servidor");
  if (!scheme || !host) return null;
  const user = f.get("usuario") ?? f.get("user") ?? "";
  const pass = f.get("password") ?? f.get("contraseña") ?? f.get("contrasena") ?? "";
  const auth = user || pass ? `${encodeURIComponent(user)}${pass ? ":" + encodeURIComponent(pass) : ""}@` : "";
  const port = f.get("puerto") ?? f.get("port");
  const db = f.get("base") ?? f.get("database") ?? f.get("db");
  return `${scheme}://${auth}${host}${port ? ":" + port : ""}${db ? "/" + db : ""}`;
}

/** The little picture under the cursor while a file is dragged out, as a PNG data URL. */
let dragIcon = "";
function dragImage(): string {
  if (dragIcon) return dragIcon;
  const c = document.createElement("canvas");
  c.width = 40;
  c.height = 48;
  const g = c.getContext("2d");
  if (!g) return "";
  g.fillStyle = "#f5f6f8";
  g.beginPath();
  g.moveTo(4, 2);
  g.lineTo(26, 2);
  g.lineTo(36, 12);
  g.lineTo(36, 46);
  g.lineTo(4, 46);
  g.closePath();
  g.fill();
  g.fillStyle = "#9398a1";
  g.fillRect(10, 20, 20, 3);
  g.fillRect(10, 27, 20, 3);
  g.fillRect(10, 34, 14, 3);
  dragIcon = c.toDataURL("image/png");
  return dragIcon;
}

export function buildLibrary(actions: ViewActions): ViewHost {
  let data: LibraryData | null = null;
  /** The project on screen; null is the grid of projects. */
  let current: string | null = null;
  /** The category on screen inside `current`; null is its three categories. */
  let category: LibraryKind | null = null;
  let filter: Filter = "todo";
  let loading = false;
  let busy = false;
  /** One line under the lists: what just happened, or Claude's reply. */
  let status = "";
  let statusKind: "info" | "ok" | "err" = "info";
  let streamed = "";
  let renderedKey = "";

  const heading = h("nav", { class: "lib-heading", "aria-label": "Ruta" });
  const folderBtn = h(
    "button",
    {
      class: "lib-top-btn",
      title: "Abrir esta carpeta en el Explorador",
      onclick: () => void Bridge.libraryOpenFolder(current, current ? category : null),
    },
    svg(ICONS.folder, 12, { stroke: 2 }),
  );
  const sortBtn = h(
    "button",
    { class: "lib-top-btn sort", title: "Claude Code ordena títulos, tipos, etiquetas y duplicados", onclick: () => void ask(SORT_PROMPT, true) },
    svg(ICONS.sparkles, 12),
    h("span", { text: "Ordenar con Claude" }),
  );
  const cols = h("div", { class: "lib-cols" });
  const statusLine = h("div", { class: "lib-status" });
  const attach = buildAttach("library");
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Guarda algo: \"guarda este prompt en coucou: …\"",
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Guardar con Claude" }, svg(ICONS.arrowUp, 11));
  const bar = h("div", { class: "chat-bar lib-bar" }, attach.button, input, send);

  const body = h(
    "div",
    { class: "lib-body" },
    h("div", { class: "lib-top" }, h("div", { class: "lib-bot-spot" }), heading, folderBtn, sortBtn),
    cols,
    statusLine,
    attach.chip,
    bar,
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

  function fail(err: unknown) {
    Sound.play("error", "chat");
    say(String(err).replace(/^Error:\s*/, ""), "err");
  }

  function go(id: string | null, kind: LibraryKind | null = null) {
    current = id;
    category = id ? kind : null;
    filter = "todo";
    renderedKey = "";
    actions.blip();
    State.notify();
  }

  async function reload() {
    if (loading) return;
    loading = true;
    try {
      data = previewData ?? (await Bridge.libraryList());
      // A project that's gone (renamed, deleted) sends you back to the grid.
      if (!data.projects.some((p) => p.id === current)) {
        current = null;
        category = null;
      }
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
      // Show where Claude just saved it: "Guardado en duo > Accesos: …".
      const m = line.match(/Guardado en\s+([^>›]+?)\s*[>›]\s*([^:]+?)\s*:/i);
      await reload();
      if (m && data) {
        const name = m[1].trim().toLowerCase();
        const hit = data.projects.find((p) => p.id.toLowerCase() === name || p.name.toLowerCase() === name);
        const cat = CATEGORIES.find((c) => c.label.toLowerCase() === m[2].trim().toLowerCase());
        if (hit) go(hit.id, cat?.kind ?? null);
      }
    } catch (err) {
      fail(err);
    } finally {
      busy = false;
      State.stateOverride = null;
      renderedKey = "";
      State.notify();
    }
  }

  async function use(item: LibraryItem, p: LibraryProject, mode: LibraryMode, done: string) {
    try {
      await Bridge.libraryUse(item.path, item.kind, mode, p.repo);
      Sound.play("approve");
      actions.emote("wink");
      say(`${p.name} › ${item.title} · ${done}`, "ok");
    } catch (err) {
      fail(err);
    }
  }

  async function copyText(text: string, what: string, p: LibraryProject) {
    try {
      await Bridge.libraryCopyText(text);
      Sound.play("approve");
      actions.emote("wink");
      say(`${p.name} › ${what} · Copiado`, "ok");
    } catch (err) {
      fail(err);
    }
  }

  /** The trash button: a first click arms it, a second within 3 s sends the file to the Recycle Bin. */
  function deleteBtn(item: LibraryItem, p: LibraryProject): HTMLElement {
    let armed = 0;
    const btn = h("button", { class: "lib-act del", title: "Borrar" }, svg(ICONS.trash, 12, { stroke: 2 }));
    const label = h("span", { text: "¿Borrar?" });
    const disarm = () => {
      window.clearTimeout(armed);
      armed = 0;
      btn.classList.remove("confirm");
      btn.title = "Borrar";
      label.remove();
    };
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (!armed) {
        Sound.play("blip");
        btn.classList.add("confirm");
        btn.title = "Clic otra vez para mandarlo a la Papelera";
        btn.append(label);
        armed = window.setTimeout(disarm, 3000);
        return;
      }
      disarm();
      void remove(item, p);
    });
    return btn;
  }

  async function remove(item: LibraryItem, p: LibraryProject) {
    try {
      await Bridge.libraryDelete(item.path);
      Sound.play("approve");
      await reload();
      say(`${p.name} › ${item.title} · a la Papelera (se puede restaurar desde ahí)`, "ok");
    } catch (err) {
      fail(err);
    }
  }

  // ── Dragging a file out ────────────────────────────────────────────────────

  /**
   * A press that moves a few pixels starts a real Windows drag of the file, so
   * it can land in a terminal (its path), a mail (an attachment) or Explorer
   * (a copy). Presses on buttons and values stay clicks.
   */
  function draggable(row: HTMLElement, item: LibraryItem, p: LibraryProject) {
    row.addEventListener("pointerdown", (down: PointerEvent) => {
      if (down.button !== 0 || (down.target as HTMLElement).closest("button, .acc-v")) return;
      const move = (e: PointerEvent) => {
        if (Math.hypot(e.clientX - down.clientX, e.clientY - down.clientY) < 6) return;
        stop();
        dragOut(row, item, p);
      };
      const stop = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", stop);
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", stop);
    });
  }

  function dragOut(row: HTMLElement, item: LibraryItem, p: LibraryProject) {
    if (State.libraryDragOut || previewData) return;
    State.libraryDragOut = true;
    row.classList.add("dragging");
    const done = (dropped: boolean) => {
      if (!State.libraryDragOut) return;
      State.libraryDragOut = false;
      row.classList.remove("dragging");
      if (dropped) {
        Sound.play("approve", "drop");
        actions.emote("wink");
        say(`${p.name} › ${item.title} · soltado`, "ok");
      }
    };
    startDrag({ item: [item.path], icon: dragImage(), mode: "copy" }, (r) => done(r.result === "Dropped"))
      .catch((err) => {
        done(false);
        fail(err);
      })
      .finally(() => window.setTimeout(() => done(false), 300));
  }

  // ── Dropping files in ──────────────────────────────────────────────────────

  /** What's under the cursor of a drag: an element of this view, or null. */
  function dropTarget(e: DragDropPayload): Element | null {
    if (!e.position) return null;
    const ratio = window.devicePixelRatio || 1;
    const hit = document.elementFromPoint(e.position.x / ratio, e.position.y / ratio);
    return hit && el.contains(hit) ? hit : null;
  }

  function markOver(target: Element | null, paths: string[] | undefined) {
    body.classList.add("dragging-file");
    for (const o of body.querySelectorAll(".over")) o.classList.remove("over");
    target?.closest(".lib-proj, .lib-cat, .lib-cols.list")?.classList.add("over");
    // A file that isn't text can only be a document.
    if (paths?.length) {
      const doc = paths.every((p) => !TEXT_EXT.test(p));
      for (const c of body.querySelectorAll<HTMLElement>(".lib-cat")) {
        c.classList.toggle("suggest", doc && c.dataset.kind === "document");
      }
    }
  }

  function clearOver() {
    body.classList.remove("dragging-file");
    for (const o of body.querySelectorAll(".over, .suggest")) o.classList.remove("over", "suggest");
  }

  dropHandler = (e) => {
    const target = dropTarget(e);
    // Over the chat bar the file is attached to the next message instead.
    if (target?.closest(".lib-bar, .attach-row")) {
      clearOver();
      return false;
    }
    if (State.fileDragOver) {
      State.fileDragOver = false;
      State.notify();
    }
    switch (e.type) {
      case "enter":
      case "over":
        markOver(target, e.paths);
        return true;
      case "leave":
        clearOver();
        return true;
      case "drop":
        clearOver();
        if (e.paths?.length) void dropFiles(e.paths, target);
        return true;
    }
    return true;
  };

  async function dropFiles(paths: string[], target: Element | null) {
    const projectCard = target?.closest<HTMLElement>(".lib-proj");
    const catCard = target?.closest<HTMLElement>(".lib-cat");
    const projectId = projectCard?.dataset.project ?? current;
    const kind = (catCard?.dataset.kind as LibraryKind | undefined) ?? (projectCard ? null : category);
    const p = data?.projects.find((x) => x.id === projectId);
    if (!p) {
      fail(data?.projects.length
        ? "Suéltalo sobre un proyecto para saber dónde guardarlo."
        : "Primero crea un proyecto: pídeselo a Claude abajo.");
      return;
    }
    try {
      const saved = await Bridge.libraryImport(paths, p.id, kind);
      Sound.play("approve", "drop");
      actions.emote("happy");
      await reload();
      const first = saved[0];
      go(p.id, first?.kind ?? kind);
      const cat = CATEGORIES.find((c) => c.kind === first?.kind);
      const what = saved.length === 1 ? first.title : `${saved.length} archivos`;
      say(`Guardado en ${p.name} › ${cat?.label ?? ""} › ${what} · el original sigue donde estaba`, "ok");
    } catch (err) {
      fail(err);
    }
  }

  // ── Rows ───────────────────────────────────────────────────────────────────

  function instructionRow(item: LibraryItem, p: LibraryProject): HTMLElement {
    const acts = h("div", { class: "lib-acts" });
    const modes = State.settings.pasteModes;
    for (const b of MODE_BUTTONS) {
      if (modes[b.key]) acts.append(iconBtn(b.icon, b.label, () => void use(item, p, b.mode, b.done)));
    }
    acts.append(iconBtn(ICONS.at, "Copiar @ruta para Claude Code", () => void use(item, p, "ref", REF_DONE)), deleteBtn(item, p));
    const row = h(
      "div",
      { class: "lib-item", title: item.preview || item.file },
      h("div", { class: "lib-item-text" },
        h("b", { text: item.title }),
        h("span", {},
          item.sub ? h("i", { class: `lib-tag ${item.sub}`, text: item.sub }) : null,
          item.preview || item.file,
        ),
      ),
      acts,
    );
    draggable(row, item, p);
    return row;
  }

  function accessCard(item: LibraryItem, p: LibraryProject): HTMLElement {
    const url = connectionUrl(item);
    const head = h(
      "div",
      { class: "acc-head" },
      h("b", { text: item.title, title: item.file }),
      item.env ? h("span", { class: "acc-env", text: item.env }) : null,
      url ? iconBtn(ICONS.link, "Copiar la URL de conexión", () => void copyText(url, `${item.title} › URL`, p)) : null,
      iconBtn(ICONS.copy, "Copiar todo", () => void use(item, p, "copy", "Copiado · pégalo con Ctrl+V")),
      iconBtn(ICONS.at, "Copiar @ruta para Claude Code", () => void use(item, p, "ref", REF_DONE)),
      deleteBtn(item, p),
    );
    const card = h("div", { class: "acc", title: item.file }, head);
    if (item.fields.length) {
      const fields = h("div", { class: "acc-fields" });
      for (const [k, v] of item.fields) {
        const value = h(
          "button",
          {
            class: "acc-v",
            title: "Clic para copiar",
            onclick: (e: Event) => {
              e.stopPropagation();
              value.classList.add("copied");
              window.setTimeout(() => value.classList.remove("copied"), 700);
              void copyText(v, `${item.title} › ${k}`, p);
            },
          },
          h("span", { text: v }),
          svg(ICONS.copy, 10, { stroke: 2 }),
        );
        fields.append(h("span", { class: "acc-k", text: k }), value);
      }
      card.append(fields);
    } else {
      card.append(h("div", { class: "acc-none", text: item.preview || "Sin líneas clave: valor todavía." }));
    }
    draggable(card, item, p);
    return card;
  }

  function documentRow(item: LibraryItem, p: LibraryProject): HTMLElement {
    const badge = docBadge(item.file);
    const icon = h("span", { class: "doc-badge", text: badge.letter });
    icon.style.background = badge.color;
    const row = h(
      "div",
      { class: "lib-item doc", title: item.file },
      icon,
      h("div", { class: "lib-item-text" },
        h("b", { text: item.title }),
        h("span", { text: [formatSize(item.size), formatDate(item.modified)].filter(Boolean).join(" · ") }),
      ),
      h("div", { class: "lib-acts" },
        iconBtn(ICONS.open, "Abrir", () => void use(item, p, "open", "Abriendo…")),
        iconBtn(ICONS.fileCopy, "Copiar el archivo · pégalo como adjunto", () =>
          void use(item, p, "file", "Archivo copiado · pégalo como adjunto con Ctrl+V")),
        iconBtn(ICONS.at, "Copiar @ruta para Claude Code", () => void use(item, p, "ref", REF_DONE)),
        deleteBtn(item, p),
      ),
    );
    draggable(row, item, p);
    return row;
  }

  // ── Levels ─────────────────────────────────────────────────────────────────

  /** "3 instrucciones · 1 acceso", leaving out what the project has none of. */
  function counts(p: LibraryProject): string {
    const parts = CATEGORIES.map((c) => {
      const n = p.items.filter((i) => i.kind === c.kind).length;
      return n > 0 ? `${n} ${n === 1 ? c.one : c.many}` : "";
    }).filter(Boolean);
    return parts.join(" · ") || "Vacío";
  }

  /** What a category card says under its name. */
  function summary(p: LibraryProject, kind: LibraryKind): string {
    const items = p.items.filter((i) => i.kind === kind);
    if (items.length === 0) return "Nada todavía";
    const n = (sub: string, one: string, many: string) => {
      const k = items.filter((i) => i.sub === sub).length;
      return k ? `${k} ${k === 1 ? one : many}` : "";
    };
    if (kind === "instruction") return [n("prompt", "prompt", "prompts"), n("entorno", "entorno", "entornos")].filter(Boolean).join(" · ");
    if (kind === "document") {
      return [n("hu", "HU", "HU"), n("plantilla", "plantilla", "plantillas"), n("otro", "otro", "otros")].filter(Boolean).join(" · ");
    }
    return items.map((i) => i.title).join(", ");
  }

  function projectCard(p: LibraryProject): HTMLElement {
    // The repo folder, only when it says something the name doesn't.
    const repo = p.repo ? lastFolder(p.repo) : "";
    const card = h(
      "button",
      { class: "lib-proj", title: p.repo ?? p.name, "data-project": p.id, onclick: () => go(p.id) },
      h("b", { text: p.name }),
      h("span", { text: counts(p) }),
      h("i", { text: repo.toLowerCase() === p.name.toLowerCase() ? "" : repo }),
      h("em", { class: "lib-drop-hint", text: `Soltar en ${p.name}` }),
    );
    card.style.setProperty("--proj", p.color ?? colorForProject(p.name));
    return card;
  }

  function categoryCard(p: LibraryProject, c: Category): HTMLElement {
    const n = p.items.filter((i) => i.kind === c.kind).length;
    const card = h(
      "button",
      { class: "lib-cat", "data-kind": c.kind, onclick: () => go(p.id, c.kind) },
      h("span", { class: "lib-cat-icon" }, c.filled ? svg(c.icon, 17) : svg(c.icon, 17, { stroke: 2 })),
      h("i", { class: "lib-cat-n", text: String(n) }),
      h("b", { text: c.label }),
      h("span", { class: "lib-cat-sum", text: summary(p, c.kind) }),
      h("em", { class: "lib-drop-hint", text: `Soltar en ${c.label}` }),
    );
    card.style.setProperty("--proj", p.color ?? colorForProject(p.name));
    return card;
  }

  function renderHeading(p: LibraryProject | undefined, c: Category | undefined) {
    clear(heading);
    const sep = () => h("span", { class: "lib-crumb-sep" }, svg(ICONS.chevronRight, 10, { stroke: 2.4 }));
    const crumb = (text: string, onClick: (() => void) | null, extra?: { color?: string; title?: string; icon?: Node }) => {
      const node = onClick
        ? h("button", { class: "lib-crumb link", title: extra?.title ?? "", onclick: onClick })
        : h("b", { class: "lib-crumb", title: extra?.title ?? "" });
      if (extra?.icon) node.append(extra.icon);
      node.append(h("span", { text }));
      if (extra?.color) node.style.setProperty("--proj", extra.color);
      if (extra?.color) node.classList.add("proj");
      return node;
    };
    heading.append(crumb("Proyectos", p ? () => go(null) : null));
    if (!p) return;
    const color = p.color ?? colorForProject(p.name);
    heading.append(sep(), crumb(p.name, c ? () => go(p.id) : null, { color, title: p.repo ?? p.name }));
    if (!c) return;
    heading.append(sep(), crumb(c.label, null, { icon: c.filled ? svg(c.icon, 12) : svg(c.icon, 12, { stroke: 2 }) }));
  }

  function render() {
    clear(cols);
    cols.className = "lib-cols";
    const projects = data?.projects ?? [];
    const p = projects.find((x) => x.id === current);
    const c = p ? CATEGORIES.find((x) => x.kind === category) : undefined;
    renderHeading(p, c);

    if (!p && projects.length > 0) {
      cols.classList.add("grid");
      for (const x of projects) cols.append(projectCard(x));
      return;
    }
    if (!p) {
      cols.classList.add("empty");
      cols.append(h(
        "div",
        { class: "lib-empty" },
        h("b", { text: loading ? "Cargando la biblioteca…" : "Tu biblioteca está vacía." }),
        h("span", {
          text: "Escríbele abajo, por ejemplo: guarda este prompt en coucou: revisa mis PRs y dime qué falta. " +
            "Todo vive en Documentos\\mochi, una carpeta por proyecto con instrucciones, accesos y documentos.",
        }),
      ));
      return;
    }
    if (!c) {
      cols.classList.add("cats");
      for (const x of CATEGORIES) cols.append(categoryCard(p, x));
      return;
    }

    cols.classList.add("list");
    const items = p.items.filter((i) => i.kind === c.kind);
    const scroll = h("div", { class: "lib-scroll" });
    if (items.length === 0) {
      scroll.append(h("div", { class: "lib-none", text: "Nada todavía · arrastra un archivo aquí o díselo a Claude abajo." }));
    } else if (c.kind === "instruction") {
      const chip = (f: Filter, label: string) =>
        h("button", {
          class: `lib-chip${filter === f ? " on" : ""}`,
          onclick: () => {
            filter = f;
            renderedKey = "";
            State.notify();
          },
        }, label);
      scroll.append(h("div", { class: "lib-chips" },
        chip("todo", `Todo · ${items.length}`), chip("prompt", "Prompts"), chip("entorno", "Entornos")));
      const rows = h("div", { class: "lib-rows" });
      for (const it of items.filter((i) => filter === "todo" || i.sub === filter)) rows.append(instructionRow(it, p));
      scroll.append(rows);
    } else if (c.kind === "access") {
      const grid = h("div", { class: "lib-grid acc-grid" });
      for (const it of items) grid.append(accessCard(it, p));
      scroll.append(grid);
    } else {
      for (const g of DOC_GROUPS) {
        const group = items.filter((i) => i.sub === g.sub);
        if (group.length === 0) continue;
        const grid = h("div", { class: "lib-grid" });
        for (const it of group) grid.append(documentRow(it, p));
        scroll.append(h("div", { class: "lib-sub", text: `${g.label} · ${group.length}` }), grid);
      }
    }
    cols.append(scroll, h("div", { class: "lib-drop-veil" }, h("b", { text: `Guardar en ${c.label}` }), h("span", { text: c.drop })));
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
      attach.sync(bar);
      const modes = State.settings.pasteModes;
      const key = [
        current, category, filter, loading,
        JSON.stringify(data?.projects.map((p) => [p.id, p.items.map((i) => i.path + i.title + i.sub + i.size + JSON.stringify(i.fields))])),
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
