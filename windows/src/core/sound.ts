// SoundEngine — port of SoundEngine.swift.
// The 28 WAVs are the macOS app's own files (see SOUNDS_DIR in vite.config.ts);
// they are served at /sounds/<name>.wav. Default volume 0.12, slider range 0–0.2,
// exactly like the Mac player, and several sounds may overlap.

export const SOUND_NAMES = [
  "peek", "open", "close", "hover", "blip", "slap", "annoyed", "dizzy", "greet",
  "work", "finish", "error", "approval", "question", "approve", "gulp", "tick",
  "send", "love", "pop", "proud", "wink", "yawn", "attach", "think", "search",
  "rate", "sleep",
] as const;

export type SoundName = (typeof SOUND_NAMES)[number];

/**
 * What a sound is about. Each one is a switch in the settings window, so you
 * choose "a session finished" or "Mochi's reactions", not a WAV file name: the
 * same file can mean different things (`finish` is a session and a chat reply).
 */
export const SOUND_CUES = [
  { id: "finish", label: "Una sesión terminó", sample: "finish", on: true },
  { id: "question", label: "Claude te pregunta algo", sample: "question", on: true },
  { id: "prompt", label: "Le enviaste un prompt a Claude Code", sample: "peek", on: true },
  { id: "approval", label: "Claude pide permiso", sample: "approval", on: false },
  { id: "start", label: "Empieza una sesión", sample: "work", on: false },
  { id: "error", label: "Una sesión se detuvo por un error", sample: "error", on: false },
  { id: "rate", label: "Llegaste al límite de uso", sample: "rate", on: true },
  { id: "rateFree", label: "Se liberó el límite de uso", sample: "pop", on: true },
  { id: "meeting", label: "Una reunión está por empezar", sample: "attach", on: true },
  { id: "github", label: "GitHub: reviews y CI de tus PRs", sample: "blip", on: true },
  { id: "integrations", label: "Otras integraciones (deploys, pagos, correos…)", sample: "finish", on: false },
  { id: "chat", label: "Chat con Claude", sample: "send", on: true },
  { id: "drop", label: "Soltar un archivo", sample: "approve", on: true },
  { id: "mochi", label: "Reacciones de Mochi", sample: "love", on: true },
  { id: "ui", label: "Abrir, cerrar y tocar la isla", sample: "open", on: false },
] as const satisfies readonly { id: string; label: string; sample: SoundName; on: boolean }[];

export type SoundCue = (typeof SOUND_CUES)[number]["id"];

/** Whether a cue plays: the user's choice, or the cue's default. */
export function cueEnabled(choices: Record<string, boolean>, id: SoundCue): boolean {
  return choices[id] ?? SOUND_CUES.find((c) => c.id === id)?.on ?? false;
}

class SoundEngine {
  enabled = true;
  volume = 0.12;
  private cues: Record<string, boolean> = {};

  private ctx: AudioContext | null = null;
  private master: GainNode | null = null;
  private buffers = new Map<string, AudioBuffer>();
  private loading: Promise<void> | null = null;
  private idleTimer: number | null = null;

  /** Creates the context and decodes every WAV. Safe to call more than once. */
  preload(): Promise<void> {
    if (this.loading) return this.loading;
    this.loading = (async () => {
      const Ctor = window.AudioContext ?? (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      if (!Ctor) return;
      const ctx = new Ctor();
      this.ctx = ctx;
      const master = ctx.createGain();
      master.gain.value = this.volume;
      master.connect(ctx.destination);
      this.master = master;
      await Promise.all(
        SOUND_NAMES.map(async (name) => {
          try {
            const res = await fetch(`/sounds/${name}.wav`);
            if (!res.ok) return;
            const buf = await ctx.decodeAudioData(await res.arrayBuffer());
            this.buffers.set(name, buf);
          } catch {
            /* a missing sound must never break the island */
          }
        }),
      );
    })();
    return this.loading;
  }

  /** WebView2 can hand us a suspended context; call after any user input. */
  resume() {
    if (this.idleTimer != null) {
      window.clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }
    void this.ctx?.resume();
  }

  /**
   * Called when the island goes quiet. A running AudioContext keeps an audio
   * thread and its render quantum alive even with nothing playing, which shows
   * up as a steady trickle of CPU on a machine that is supposed to be idle.
   *
   * The delay covers the tail of whatever just played — suspending mid-sound
   * would clip it — and `play()` resumes the context on its own.
   */
  idle() {
    if (!this.ctx || this.ctx.state !== "running" || this.idleTimer != null) return;
    this.idleTimer = window.setTimeout(() => {
      this.idleTimer = null;
      void this.ctx?.suspend();
    }, 1500);
  }

  setVolume(v: number) {
    this.volume = Math.max(0, Math.min(0.2, v));
    if (this.master) this.master.gain.value = this.volume;
  }

  setEnabled(on: boolean) {
    this.enabled = on;
  }

  /** The settings window's per-cue switches. */
  setCues(choices: Record<string, boolean>) {
    this.cues = { ...choices };
  }

  /**
   * Plays `name` if sound is on and its cue is switched on. Calls without a cue
   * are island chrome (`ui`), which is how upstream's call sites read.
   */
  play(name: SoundName, cue: SoundCue = "ui") {
    if (!this.enabled || !cueEnabled(this.cues, cue)) return;
    this.start(name);
  }

  /** The settings window's ▶ button: plays even when the cue is off. */
  async preview(name: SoundName) {
    await this.preload();
    this.start(name);
  }

  private start(name: SoundName) {
    const ctx = this.ctx;
    const master = this.master;
    const buf = this.buffers.get(name);
    if (!ctx || !master || !buf) return;
    if (this.idleTimer != null) {
      window.clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }
    if (ctx.state === "suspended") void ctx.resume();
    const src = ctx.createBufferSource();
    src.buffer = buf;
    src.connect(master);
    src.start();
  }
}

export const Sound = new SoundEngine();
