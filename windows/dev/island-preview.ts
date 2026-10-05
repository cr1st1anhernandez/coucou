// Dev harness: opens the real island on one view with made-up sessions, so a
// card can be looked at (and screenshotted) without Claude Code or Tauri.
// `dev/island-preview.html#finished` — the scene is the URL hash. Not bundled.

import "../src/style.css";

// Headless Edge throttles requestAnimationFrame; a timer frame lets
// --virtual-time-budget run the springs to rest before the screenshot.
window.requestAnimationFrame = (cb) => window.setTimeout(() => cb(performance.now()), 16);
import { State } from "../src/core/state";
import { Island } from "../src/island/island";
import {
  appendStep, makeCurrent, queuePrompt, recordTodos, setRateLimited, setStatus, startSubagent, stopSubagent,
  touchSession,
} from "../src/island/sessions";
import { setLibraryPreview } from "../src/views/library";

type Scene = (island: Island) => void;

function session(id: string, name: string) {
  const s = touchSession(id, `C:/Users/dev/Projects/${name}`, name);
  appendStep(s, "Edita · views.ts");
  return s;
}

const SCENES: Record<string, Scene> = {
  finished(island) {
    const s = session("a", "coucou");
    s.summary = {
      files: [
        { path: "C:\\Users\\dev\\Projects\\coucou\\windows\\src\\views\\views.ts", added: 48, removed: 6 },
        { path: "C:\\Users\\dev\\Projects\\coucou\\windows\\src\\island\\hooks.ts", added: 12, removed: 3 },
        { path: "C:\\Users\\dev\\Projects\\coucou\\windows\\src\\style.css", added: 40, removed: 0 },
        { path: "C:\\Users\\dev\\Projects\\coucou\\windows\\src\\core\\layout.ts", added: 9, removed: 2 },
      ],
      added: 109, removed: 11, tests: "passed",
    };
    s.finalMessage = "Listo, la tarjeta de terminó ahora muestra cada archivo.";
    setStatus(s, "finished");
    makeCurrent(s.id);
    island.alert("finished");
    island.celebrate();
  },
  question(island) {
    const s = session("a", "coucou");
    State.pendingQuestion = {
      requestId: "1", sessionId: s.id, step: 0, answers: {},
      questions: [{
        question: "¿Qué base de datos usamos para las sesiones?",
        header: "Base de datos",
        multiSelect: false,
        options: [
          { label: "SQLite", description: "Un archivo, sin servidor" },
          { label: "Postgres", description: "La que ya usa el backend" },
          { label: "En memoria", description: "Se pierde al reiniciar" },
        ],
      }],
    };
    setStatus(s, "question");
    makeCurrent(s.id);
    island.alert("question");
  },
  "question-multi"(island) {
    const s = session("a", "coucou");
    State.pendingQuestion = {
      requestId: "1", sessionId: s.id, step: 1, answers: { "x": "y" },
      questions: [
        { question: "x", header: "", multiSelect: false, options: [{ label: "y", description: "" }] },
        {
          question: "¿Qué pruebas corro antes de subir los cambios a la rama de la computadora del trabajo?",
          header: "Pruebas",
          multiSelect: true,
          options: [
            { label: "tsc", description: "Tipos del front" },
            { label: "cargo check", description: "Rust" },
            { label: "cargo test", description: "Pruebas del relay" },
            { label: "Instalar", description: "Compilar el instalador" },
          ],
        },
      ],
    };
    setStatus(s, "question");
    makeCurrent(s.id);
    island.alert("question");
    window.setTimeout(() => (document.querySelectorAll(".view.on .option")[1] as HTMLElement)?.click(), 300);
  },
  github(island) {
    State.integrations.integration_github = {
      loaded: true, configured: true, error: null,
      data: {
        totalRepos: 12, totalStars: 40,
        prs: [
          { title: "Windows: answer Claude's questions from the island", repo: "cr1st1anhernandez/coucou", number: 7, url: "https://github.com", status: "ci_failed" },
          { title: "Rate limit countdown", repo: "cr1st1anhernandez/coucou", number: 6, url: "https://github.com", status: "approved" },
          { title: "Sound toggles", repo: "cr1st1anhernandez/coucou", number: 5, url: "https://github.com", status: "ci_running" },
        ],
      },
    };
    State.setFocus("integration_github");
    island.alert("overview");
  },
  ratelimit(island) {
    const s = session("a", "coucou");
    setRateLimited(s, Date.now() + 82 * 60_000);
    makeCurrent(s.id);
    island.alert("ratelimit");
  },
  "rate-free"(island) {
    session("a", "coucou");
    island.announce("Ya se liberó el límite de uso · coucou");
  },
  chat(island) {
    State.chatHistory = [
      { id: 1, role: "user", content: "¿Qué cambió en el último merge a main?" },
      { id: 2, role: "assistant", content: "Se quitaron las integraciones que no usabas; quedan GitHub y Warp." },
    ];
    State.attachments.chat = { name: "accesos-staging.md", path: "C:\\Users\\dev\\AppData\\Local\\Coucou\\inbox\\accesos-staging.md" };
    island.alert("prompt");
  },
  "chat-empty"(island) {
    island.alert("prompt");
  },
  "chat-streaming"(island) {
    State.chatHistory = [
      { id: 1, role: "user", content: "Resume este archivo", attachment: "plan.pdf" },
      { id: 2, role: "assistant", content: "", status: "Leyendo el archivo…" },
    ];
    island.alert("prompt");
  },
  library(island) {
    const base = "C:\\Users\\dev\\Documents\\mochi";
    type Kind = "instruction" | "access" | "document";
    const folder = { instruction: "instrucciones", access: "accesos", document: "documentos" };
    const item = (kind: Kind, sub: string, project: string, file: string, title: string, preview: string,
      more: { fields?: [string, string][]; size?: number } = {}) =>
      ({
        kind, sub, title, file, path: `${base}\\${project}\\${folder[kind]}\\${file}`, preview, tags: [], order: 1,
        fields: more.fields ?? [], env: kind === "access" ? "dev" : "", size: more.size ?? 1200, modified: Date.now(),
      });
    setLibraryPreview({
      dir: base,
      projects: [
        {
          id: "coucou", name: "coucou", color: "#8B5CF6", repo: "C:\\Users\\dev\\Projects\\coucou",
          items: [
            item("instruction", "prompt", "coucou", "forma-de-trabajo.md", "Forma de trabajo", "Rama feat/*, tsc y cargo check sin advertencias, commits en inglés."),
            item("instruction", "prompt", "coucou", "revisar-prs.md", "Revisa mis PRs", "Revisa mis PRs abiertos y dime qué falta para mergear cada uno."),
            item("instruction", "entorno", "coucou", "levantar-windows.md", "Levantar Windows en dev", "cd windows && npm i && npm run tauri dev"),
            item("access", "", "coucou", "postgres-dev.md", "Postgres dev", "", {
              fields: [["host", "localhost"], ["puerto", "5432"], ["base", "coucou_dev"], ["usuario", "postgres"], ["password", "dev1234"]],
            }),
            item("access", "", "coucou", "redis-dev.md", "Redis dev", "", { fields: [["host", "localhost"], ["puerto", "6379"]] }),
            item("document", "hu", "coucou", "HU-012 Login con Google.docx", "HU-012 Login con Google", "", { size: 48_000 }),
            item("document", "plantilla", "coucou", "Plantilla HU.docx", "Plantilla HU", "", { size: 22_000 }),
          ],
        },
        { id: "api-pagos", name: "api-pagos", color: "#22C55E", repo: null, items: [] },
        { id: "web", name: "web", color: "#F5A524", repo: null, items: [] },
      ],
    });
    island.alert("library");
    window.setTimeout(() => island["views"].get("library")?.focus?.(), 50);
    // `?project` opens the first card instead of staying on the grid.
    if (location.search.includes("project")) {
      window.setTimeout(() => (document.querySelector(".lib-proj") as HTMLElement | null)?.click(), 300);
    }
    // `?project&cat=access` goes one level further, into that category.
    const cat = new URLSearchParams(location.search).get("cat");
    if (cat) {
      window.setTimeout(() => (document.querySelector(`.lib-cat[data-kind="${cat}"]`) as HTMLElement | null)?.click(), 600);
    }
  },
  // A burst of steps, then the turn's last reply, the way a real turn ends.
  ticker(island) {
    const s = session("a", "coucou");
    setStatus(s, "working");
    makeCurrent(s.id);
    island.alert("overview");
    const steps = ["Lee · ticker.ts", "Edita · views.ts", "Ejecuta · cd /c/Users/dev/claude/projects && git status --short"];
    steps.forEach((t, i) => window.setTimeout(() => { appendStep(s, t); State.notify(); }, 400 + i * 60));
    window.setTimeout(() => {
      appendStep(s, "Ya uní las dos ramas a main y la instalé.");
      setStatus(s, "finished");
      State.notify();
    }, 700);
    // `?away` leaves the overview mid-scroll and comes back, as the finished
    // card or the auto-close do in real use.
    if (location.search.includes("away")) {
      window.setTimeout(() => island.alert("finished"), 760);
      window.setTimeout(() => island.alert("overview"), 1600);
    }
    if (location.search.includes("hide")) {
      window.setTimeout(() => { State.isPinned = false; island.collapse(); }, 760);
      window.setTimeout(() => island.alert("overview"), 2600);
    }
  },
  subagents(island) {
    const s = session("a", "coucou");
    setStatus(s, "working");
    startSubagent(s, "1", "Explore");
    startSubagent(s, "2", "general-purpose");
    makeCurrent(s.id);
    island.alert("overview");
    if (location.search.includes("done")) window.setTimeout(() => { stopSubagent(s, "1"); State.notify(); }, 800);
  },
  todo(island) {
    const s = session("a", "coucou");
    setStatus(s, "working");
    recordTodos(s, { todos: [
      { status: "completed" }, { status: "completed" }, { status: "completed" },
      { status: "in_progress" }, { status: "pending" }, { status: "pending" }, { status: "pending" },
    ] });
    makeCurrent(s.id);
    island.alert("overview");
  },
  "todo-compact"(island) {
    const s = session("a", "coucou");
    setStatus(s, "working");
    recordTodos(s, { todos: [{ status: "completed" }, { status: "pending" }, { status: "pending" }] });
    makeCurrent(s.id);
    State.isPinned = false;
    island.collapse();
  },
  knock(island) {
    const s = session("a", "coucou");
    setStatus(s, "question");
    makeCurrent(s.id);
    State.isPinned = false;
    island.collapse();
    window.setTimeout(() => island.nudge(false), 400);
  },
  focus(island) {
    const s = session("a", "coucou");
    setStatus(s, "working");
    makeCurrent(s.id);
    island.alert("overview");
    island.setFocusMode(true);
    if (location.search.includes("done")) window.setTimeout(() => island.setFocusMode(false, true), 900);
  },
  away(island) {
    const a = session("a", "coucou");
    a.summary = { files: [{ path: "x.ts", added: 40, removed: 3 }], added: 40, removed: 3, tests: "passed" };
    setStatus(a, "finished");
    const b = session("b", "api-pagos");
    setStatus(b, "error");
    appendStep(b, "3 tests fallan en checkout.spec.ts");
    const c = session("c", "web");
    setStatus(c, "approval");
    State.away = {
      minutes: 42,
      rows: [
        { sessionId: "c", name: "web", status: "approval", detail: "Espera permiso · npm install zod" },
        { sessionId: "b", name: "api-pagos", status: "error", detail: "Error · 3 tests fallan en checkout.spec.ts" },
        { sessionId: "a", name: "coucou", status: "finished", detail: "Terminó · 1 archivo · +40 −3 · tests ✓" },
      ],
    };
    island.showAway();
  },
  queue(island) {
    const s = session("a", "coucou");
    setStatus(s, "working");
    if (!location.search.includes("empty")) {
      queuePrompt(s, "Ahora corre tsc y cargo check, y arregla lo que falle sin advertencias nuevas");
      queuePrompt(s, "Haz commit");
    }
    makeCurrent(s.id);
    island.alert("queue");
  },
  "queue-overview"(island) {
    const s = session("a", "coucou");
    setStatus(s, "working");
    if (location.search.includes("queued")) queuePrompt(s, "Haz commit");
    makeCurrent(s.id);
    island.alert("overview");
  },
  "library-empty"(island) {
    setLibraryPreview({ dir: "C:\\Users\\dev\\Documents\\mochi", projects: [] });
    island.alert("library");
    window.setTimeout(() => island["views"].get("library")?.focus?.(), 50);
  },
  "finished-empty"(island) {
    const s = session("a", "coucou");
    s.finalMessage = "Es un error en la tarjeta, no algo que Claude te esté diciendo.";
    setStatus(s, "finished");
    makeCurrent(s.id);
    island.alert("finished");
  },
};

const root = document.getElementById("root")!;
const island = new Island(root);
State.loadIntegrationTasks();
island.applySettings();
// Pinned, so the auto-close never folds the card before the screenshot.
State.isPinned = true;
const scene = SCENES[location.hash.slice(1)] ?? SCENES.finished;
scene(island);
State.notify();
