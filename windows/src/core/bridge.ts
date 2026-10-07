// Thin wrapper over the Tauri commands/events. Every call is a no-op when the
// page is opened in a plain browser, so the island can be iterated on with
// `npm run dev` alone.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Settings } from "./state";
import type { PhoneSession } from "../island/phone";

export const IS_TAURI =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T | null> {
  if (!IS_TAURI) return null;
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    console.error(`[coucou] ${cmd} failed`, err);
    return null;
  }
}

export interface BootInfo {
  settings: Settings;
  /** Logical screen rect of the monitor the island lives on. */
  screen: { x: number; y: number; width: number; height: number; scale: number };
  version: string;
  hookPath: string;
}

/** What became of a queue_deliver (pipe::Delivery). */
export type Delivery = "sent" | "stale" | "user-active" | "no-terminal" | "failed";

export const Bridge = {
  boot: () => call<BootInfo>("boot"),

  saveSettings: (settings: Settings) => call<void>("save_settings", { settings }),

  /** Shrink the window down to the invisible wake strip (hidden) or back to full. */
  setCollapsed: (collapsed: boolean) => call<void>("set_collapsed", { collapsed }),

  /**
   * Pushes the island shape in window coordinates. Rust flips click-through from
   * its own cursor poll, so the flag is never a frame behind a click.
   */
  setIslandRect: (x: number, y: number, width: number, height: number) =>
    call<void>("set_island_rect", { x, y, width, height }),

  /** Give the window keyboard focus (chat field) and take it away again. */
  focusWindow: (focused: boolean) => call<void>("focus_window", { focused }),

  reposition: () => call<void>("reposition"),

  openUrl: (url: string) => call<void>("open_url", { url }),

  /**
   * "Open terminal" → the session's own Warp tab when `focusUrl` (WARP_FOCUS_URL)
   * is known, Warp's front window otherwise; Explorer when Warp isn't installed.
   */
  openTerminal: (path: string | null, focusUrl: string | null = null) =>
    call<boolean>("open_terminal", { path, focusUrl }),

  quit: () => call<void>("quit_app"),

  openSettingsWindow: () => call<void>("open_settings_window"),

  /** Writes to %LOCALAPPDATA%\Coucou\coucou.log, next to the Rust lines. */
  log: (message: string) => call<void>("log_line", { message }),

  // ── Claude Code hooks ─────────────────────────────────────────────────────
  hooksStatus: () => call<HookStatus>("hooks_status"),
  /** Diff to show before anything is written. `install: false` previews removal. */
  hooksPreview: (install: boolean) => callOrThrow<HookPreview>("hooks_preview", { install }),
  /**
   * Writes ~/.claude/settings.json — only ever after an explicit click, and only
   * when the file still matches the preview the user looked at.
   */
  hooksApply: (install: boolean, fingerprint: string) =>
    callOrThrow<string>("hooks_apply", { install, fingerprint }),

  approvalDecision: (requestId: string, decision: "allow" | "deny") =>
    call<void>("approval_decision", { requestId, decision }),
  /** An AskUserQuestion answered from the island: question text → chosen label(s). */
  questionAnswer: (requestId: string, answers: Record<string, string>) =>
    call<void>("question_answer", { requestId, answers }),
  /** "The card is up" — until this lands the relay only waits a moment. */
  approvalAck: (requestId: string) => call<void>("approval_ack", { requestId }),
  /** "Nobody can act on this" — Claude Code asks in the terminal right away. */
  approvalDecline: (requestId: string) => call<void>("approval_decline", { requestId }),
  /** The prompts queued for a session; Coucou sends the first one when its turn ends. */
  queueSet: (sessionId: string, prompts: string[]) =>
    call<void>("queue_set", { sessionId, prompts }),
  /** Types the first queued prompt into the terminal of a session that isn't working. */
  queueDeliver: (sessionId: string, prompt: string, away: boolean) =>
    call<Delivery>("queue_deliver", { sessionId, prompt, away }),

  // ── Chat, files, secrets ──────────────────────────────────────────────────
  /** One chat turn. The API key and any file bytes never leave Rust. */
  chatSend: (query: string, context: ChatContext | null) =>
    callOrThrow<{ text: string }>("chat_send", { query, context }),
  chatReset: () => call<void>("chat_reset"),
  /**
   * One turn on the user's own Claude Code (their account, no API key). The
   * reply also streams in as `code-chat` events while it's being written.
   */
  codeChat: (channel: CodeChannel, prompt: string, attachments: string[]) =>
    callOrThrow<{ text: string }>("code_chat", { channel, prompt, attachments }),
  codeChatReset: (channel: CodeChannel) => call<void>("code_chat_reset", { channel }),
  /** The Windows Open dialog; the file is copied into the inbox. Null on cancel. */
  pickFile: () => callOrThrow<DroppedFile | null>("pick_file"),

  // ── Library (<Documents>\mochi) ───────────────────────────────────────────
  libraryList: () => callOrThrow<LibraryData>("library_list"),
  /**
   * "copy" its text, "ref" `@"path"`, "warp" copy + Warp, "paste" copy + Warp + Ctrl+V,
   * "file" the file itself (pastes as an attachment), "open" in its own app.
   */
  libraryUse: (path: string, kind: LibraryKind, mode: LibraryMode, repo: string | null) =>
    callOrThrow<void>("library_use", { path, kind, mode, repo }),
  /** One value of an access card, or its connection URL. */
  libraryCopyText: (text: string) => callOrThrow<void>("library_copy_text", { text }),
  /** Copies dropped files into a project — into `kind` when dropped on a category. */
  libraryImport: (paths: string[], project: string, kind: LibraryKind | null) =>
    callOrThrow<LibraryImported[]>("library_import", { paths, project, kind }),
  /** Sends a saved item to the Recycle Bin; gives back its file name. */
  libraryDelete: (path: string) => callOrThrow<string>("library_delete", { path }),
  /** Explorer on the library, or on the project / category on screen. */
  libraryOpenFolder: (project: string | null, kind: LibraryKind | null) =>
    call<void>("library_open_folder", { project, kind }),

  /** Seconds since the last keyboard or mouse input anywhere. */
  idleSeconds: () => call<number>("idle_seconds"),
  /** Copies a dropped file into the inbox. */
  ingestFile: (path: string) => callOrThrow<DroppedFile>("ingest_file", { path }),
  /** Only ever tells you whether a key exists — never its value. */
  secretPresent: (key: string) => call<boolean>("secret_present", { key }),
  secretSet: (key: string, value: string) => callOrThrow<void>("secret_set", { key, value }),
  secretClear: (key: string) => callOrThrow<void>("secret_clear", { key }),

  // ── Integrations ──────────────────────────────────────────────────────────
  refreshIntegration: (id: string) => call<void>("refresh_integration", { id }),

  /** Tray → Pause. Stops the integration pollers, not just the island. */
  setPaused: (paused: boolean) => call<void>("set_paused", { paused }),

  // ── iPhone (phone.rs) ─────────────────────────────────────────────────────
  /** Server state and the Tailscale URL, for the settings window. */
  phoneStatus: () => call<PhoneStatus>("phone_status"),
  /** The island's sessions, for the phones. Only sent when something changed. */
  phonePublish: (sessions: PhoneSession[], away: boolean) =>
    call<void>("phone_publish", { sessions, away }),
  /** A one-time 6-digit code, valid 5 minutes; a new one replaces the last. */
  phonePairCode: () => call<{ code: string; expiresAt: number }>("phone_pair_code"),
  phoneDevices: () => call<PhoneDevice[]>("phone_devices"),
  /** Forgets a paired phone; its open connections close at once. */
  phoneRevoke: (deviceId: string) => call<void>("phone_revoke", { deviceId }),
};

export interface PhoneDevice {
  deviceId: string;
  device: string;
  createdAt: number;
  lastSeen: number;
  /** It allowed notifications. */
  push: boolean;
}

export interface PhoneStatus {
  running: boolean;
  /** Why the server couldn't start, e.g. the port is taken. */
  error: string | null;
  port: number;
  /** `https://<pc>.<tailnet>.ts.net`, when Tailscale can tell. */
  url: string | null;
  tailscale: boolean;
  webDir: string;
  webInstalled: boolean;
}

export interface IntegrationUpdate {
  id: string;
  data: Record<string, unknown>;
  error: string | null;
  event: { success: boolean; label: string; detail: string | null } | null;
}

export type ChatContext =
  | { kind: "file"; name: string; path: string }
  | { kind: "window"; appName: string; title: string; url?: string };

export interface DroppedFile {
  name: string;
  path: string;
  size: number;
}

export type CodeChannel = "chat" | "library";

/** A `code-chat` event: more reply text, or what Claude is doing right now. */
export interface CodeChatEvent {
  channel: CodeChannel;
  kind: "delta" | "status";
  text: string;
}

export type LibraryKind = "instruction" | "access" | "document";
export type LibraryMode = "copy" | "ref" | "warp" | "paste" | "file" | "open";

export interface LibraryItem {
  kind: LibraryKind;
  /** Instructions: "prompt" | "entorno". Documents: "hu" | "plantilla" | "otro". Accesses: "". */
  sub: string;
  title: string;
  file: string;
  path: string;
  preview: string;
  tags: string[];
  order: number;
  /** An access's `clave: valor` lines. */
  fields: [string, string][];
  /** An access's environment: "dev", "qa"… */
  env: string;
  size: number;
  /** Milliseconds since 1970. */
  modified: number | null;
}

export interface LibraryImported {
  kind: LibraryKind;
  title: string;
  path: string;
}

export interface LibraryProject {
  id: string;
  name: string;
  color: string | null;
  repo: string | null;
  items: LibraryItem[];
}

export interface LibraryData {
  dir: string;
  projects: LibraryProject[];
}

export interface HookStatus {
  installed: boolean;
  settingsPath: string;
  hookPath: string;
  hookReady: boolean;
}

export interface HookPreview {
  diff: string;
  backup: string;
  settingsPath: string;
  /** Hand back to hooksApply so only the reviewed diff is ever written. */
  fingerprint: string;
}

/** Same as `call`, but surfaces the error so the UI can show what went wrong. */
async function callOrThrow<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!IS_TAURI) throw new Error("not running inside Coucou");
  return invoke<T>(cmd, args);
}

export type BridgeEvent =
  | { name: "cursor"; payload: { x: number; y: number } }
  | { name: "tray"; payload: string }
  | { name: "hook"; payload: Record<string, unknown> }
  | { name: "screen-changed"; payload: null };

export interface DragDropPayload {
  type: "enter" | "over" | "drop" | "leave";
  paths?: string[];
  /** Where the cursor is, in physical pixels from the webview's top left. */
  position?: { x: number; y: number };
}

/**
 * Files dragged onto the island. Coucou's own drop target sends them (see
 * drop_target.rs): Tauri's never received any. Only reaches us when the window
 * takes the mouse.
 */
export async function onDragDrop(handler: (e: DragDropPayload) => void) {
  return onEvent<DragDropPayload>("file-drag", handler);
}

export async function onEvent<T>(name: string, handler: (payload: T) => void) {
  if (!IS_TAURI) return () => {};
  return listen<T>(name, (e) => handler(e.payload));
}
