// Chat view — DOM port of PromptView / ChatBubble / TypingDotsView from
// IslandViewContent.swift.
//
// It runs on the user's own Claude Code by default (their account, no API key),
// streaming the reply as it's written; the API key engine is still there for
// whoever prefers it (Ajustes → Claude → Motor del chat). Files go in through
// the paperclip in the input bar and ride along with the next message.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { buildAttach } from "./attach";
import { Bridge, onEvent, type ChatContext, type CodeChatEvent } from "../core/bridge";
import { Sound } from "../core/sound";
import { State, type ChatMessage } from "../core/state";
import type { ViewHost } from "./views";

let nextId = 1;

function bubble(message: ChatMessage): HTMLElement {
  if (message.role === "user") {
    const b = h("div", { class: "bubble" });
    if (message.attachment) {
      b.append(h("span", { class: "bubble-file" }, svg(ICONS.doc, 10), h("span", { text: message.attachment })));
    }
    if (message.content) b.append(document.createTextNode(message.content));
    return h("div", { class: "chat-row user" }, b);
  }
  if (!message.content) {
    // Still waiting for the first words: the dots, and what Claude is up to.
    return h(
      "div",
      { class: "chat-row" },
      h("div", { class: "typing" }, h("i"), h("i"), h("i")),
      message.status ? h("span", { class: "typing-status", text: message.status }) : null,
    );
  }
  return h("div", { class: "chat-row" }, h("div", { class: "reply", text: message.content }));
}

export function buildPrompt(onHeightChange: () => void): ViewHost {
  const log = h("div", { class: "chat-log" });
  const attach = buildAttach("chat");
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Pregúntame lo que quieras…",
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Enviar" }, svg(ICONS.arrowUp, 11));
  const bar = h("div", { class: "chat-bar" }, attach.button, input, send);

  const el = h(
    "div",
    { class: "view" },
    h("div", { class: "card wash chat-card" }, h("div", { class: "chat-body" }, log, attach.chip, bar)),
  );
  (el.querySelector(".card") as HTMLElement).style.setProperty("--wash", "rgba(99,102,241,0.5)");

  let sending = false;
  let renderedKey = "";
  /** The assistant message the stream is writing into. */
  let streaming: ChatMessage | null = null;

  void onEvent<CodeChatEvent>("code-chat", (e) => {
    if (e.channel !== "chat" || !streaming) return;
    if (e.kind === "delta") {
      streaming.content += e.text;
      streaming.status = undefined;
    } else if (!streaming.content) {
      streaming.status = e.text;
    }
    State.notify();
  });

  async function submit() {
    const query = input.value.trim();
    const file = State.attachments.chat;
    if ((!query && !file) || sending) return;
    input.value = "";
    sending = true;
    Sound.play("send", "chat");

    const asked = query || "¿Qué hay en este archivo?";
    State.chatHistory.push({ id: nextId++, role: "user", content: query, attachment: file?.name });
    State.attachments.chat = null;
    const reply: ChatMessage = { id: nextId++, role: "assistant", content: "", status: "Pensando…" };
    State.chatHistory.push(reply);
    streaming = reply;
    State.stateOverride = "thinking";
    State.notify();
    onHeightChange();

    try {
      if (State.settings.chatEngine === "api") {
        const context: ChatContext | null = file ? { kind: "file", name: file.name, path: file.path } : null;
        const r = await Bridge.chatSend(asked, context);
        reply.content = r.text;
      } else {
        const r = await Bridge.codeChat("chat", asked, file ? [file.path] : []);
        // The stream already wrote most of it; the final text is the source of truth.
        reply.content = r.text || reply.content;
      }
      reply.status = undefined;
      State.stateOverride = null;
      Sound.play("finish", "chat");
    } catch (err) {
      State.chatHistory = State.chatHistory.filter((m) => m !== reply);
      State.stateOverride = null;
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      State.view = "note";
      Sound.play("error", "chat");
    } finally {
      streaming = null;
      sending = false;
      State.notify();
      onHeightChange();
      input.focus();
    }
  }

  send.addEventListener("click", () => void submit());
  input.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") {
      e.preventDefault();
      void submit();
    }
    e.stopPropagation(); // Escape closes the island, not the chat
  });

  return {
    el,
    sync() {
      attach.sync(bar);
      const key = State.chatHistory.map((m) => `${m.id}:${m.content.length}:${m.status ?? ""}`).join("|");
      if (key !== renderedKey) {
        renderedKey = key;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        log.scrollTop = log.scrollHeight;
      }
      input.placeholder = State.chatHistory.length === 0 ? "Pregúntame lo que quieras…" : "Continúa…";
      input.disabled = sending;
    },
    focus() {
      input.focus();
      input.select();
    },
  };
}
