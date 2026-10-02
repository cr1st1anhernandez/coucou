// "Suelta tus archivos aquí" inside a chat — the drop view's dashed, marching
// frame in a compact strip. Click it for the Windows Open dialog, or drag a
// file onto the island (island.ts routes the drop here while a chat is up).
// Once a file is attached the strip turns into a chip with the file's name and
// a × to take it off again; it goes out with the next message.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { dashedFrame } from "./upload";
import { Bridge } from "../core/bridge";
import { Sound } from "../core/sound";
import { State, type Attachment } from "../core/state";

export type AttachChannel = "chat" | "library";

export interface DropZone {
  el: HTMLElement;
  sync(): void;
}

export function attach(channel: AttachChannel, file: Attachment | null) {
  State.attachments[channel] = file;
  State.notify();
}

export function buildDropZone(channel: AttachChannel, tags: string[]): DropZone {
  const title = h("span", { class: "drop-mini-title", text: "Suelta tus archivos aquí" });
  const hint = h("span", { class: "drop-mini-hint", text: "o haz clic para elegir" });
  const tagRow = h("span", { class: "drop-tags" }, ...tags.map((t) => h("span", { text: t })));
  const zone = h(
    "button",
    { class: "card drop-card drop-mini", title: "Adjuntar un archivo" },
    dashedFrame(12),
    h("span", { class: "drop-mini-icon" }, svg(ICONS.upload, 13, { stroke: 2 })),
    h("span", { class: "drop-mini-text" }, title, hint),
    tagRow,
  );
  zone.addEventListener("click", async () => {
    Sound.play("blip");
    try {
      const file = await Bridge.pickFile();
      if (!file) return;
      Sound.play("approve", "drop");
      attach(channel, { name: file.name, path: file.path });
    } catch (err) {
      Sound.play("error", "drop");
      zone.title = String(err).replace(/^Error:\s*/, "");
    }
  });

  const chip = h("div", { class: "attach-chip" });
  const el = h("div", { class: "drop-mini-wrap" }, zone, chip);
  let shown = "";

  return {
    el,
    sync() {
      const file = State.attachments[channel];
      const over = State.fileDragOver && (State.view === "prompt" ? "chat" : "library") === channel;
      zone.classList.toggle("over", over);
      title.textContent = over ? "Suéltalo" : "Suelta tus archivos aquí";
      // Dragging another file over replaces the one attached, so show the zone.
      zone.style.display = file && !over ? "none" : "";
      chip.style.display = file && !over ? "" : "none";
      const key = file ? `${file.name}|${file.path}` : "";
      if (key === shown) return;
      shown = key;
      clear(chip);
      if (file) {
        chip.append(
          svg(ICONS.doc, 11),
          h("span", { class: "attach-name", text: file.name, title: file.path }),
          h("button", {
            class: "attach-x",
            title: "Quitar",
            onclick: () => {
              Sound.play("blip");
              attach(channel, null);
            },
          }, svg(ICONS.xmark, 9)),
        );
      }
    },
  };
}
