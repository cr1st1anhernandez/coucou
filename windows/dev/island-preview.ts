// Dev harness: opens the real island on one view with made-up sessions, so a
// card can be looked at (and screenshotted) without Claude Code or Tauri.
// `dev/island-preview.html#finished` — the scene is the URL hash. Not bundled.

import "../src/style.css";

// Headless Edge throttles requestAnimationFrame; a timer frame lets
// --virtual-time-budget run the springs to rest before the screenshot.
window.requestAnimationFrame = (cb) => window.setTimeout(() => cb(performance.now()), 16);
import { State } from "../src/core/state";
import { Island } from "../src/island/island";
import { appendStep, makeCurrent, setRateLimited, setStatus, touchSession } from "../src/island/sessions";

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
