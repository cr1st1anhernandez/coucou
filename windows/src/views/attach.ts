// "Adjuntar archivo" in a chat's input bar: a paperclip button that opens the
// Windows Open dialog. Dragging a file onto the island while a chat is up
// works too (island.ts routes the drop here) and lights the bar up. Once a
// file is attached it shows as a chip above the bar, with a × to take it off;
// it goes out with the next message.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { Bridge } from "../core/bridge";
import { Sound } from "../core/sound";
import { State, type Attachment } from "../core/state";

export type AttachChannel = "chat" | "library";

export interface AttachControl {
  /** The paperclip, to put first in the input bar. */
  button: HTMLElement;
  /** The attached file, to put above the bar. Empty when there is none. */
  chip: HTMLElement;
  /** Highlights `bar` while a file is dragged over the island. */
  sync(bar: HTMLElement): void;
}

export function attach(channel: AttachChannel, file: Attachment | null) {
  State.attachments[channel] = file;
  State.notify();
}

export function buildAttach(channel: AttachChannel): AttachControl {
  const button = h(
    "button",
    { class: "attach-btn", title: "Adjuntar archivo" },
    svg(ICONS.paperclip, 14, { stroke: 2 }),
  );
  button.addEventListener("click", async () => {
    Sound.play("blip");
    try {
      const file = await Bridge.pickFile();
      if (!file) return;
      Sound.play("approve", "drop");
      attach(channel, { name: file.name, path: file.path });
    } catch (err) {
      Sound.play("error", "drop");
      button.title = String(err).replace(/^Error:\s*/, "");
    }
  });

  const chip = h("div", { class: "attach-row" });
  let shown = "";

  return {
    button,
    chip,
    sync(bar: HTMLElement) {
      const over = State.fileDragOver && (State.view === "prompt" ? "chat" : "library") === channel;
      bar.classList.toggle("over", over);
      const file = State.attachments[channel];
      const key = file ? `${file.name}|${file.path}` : "";
      if (key === shown) return;
      shown = key;
      clear(chip);
      if (!file) return;
      chip.append(h(
        "div",
        { class: "attach-chip" },
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
      ));
    },
  };
}
