// Chat view — DOM port of PromptView / ChatBubble / TypingDotsView from
// IslandViewContent.swift.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { Bridge, type ChatContext } from "../core/bridge";
import { Sound } from "../core/sound";
import { State, type ChatMessage } from "../core/state";
import type { ViewHost } from "./views";

let nextId = 1;

/**
 * Set while the chat view exists. What was said into the microphone is submitted
 * through the very function a typed message goes through, so a spoken request and a
 * typed one are the same request.
 */
let submitText: ((text: string) => void) | null = null;

export function submitVoiceQuery(text: string) {
  submitText?.(text);
}

function bubble(message: ChatMessage): HTMLElement {
  if (message.role === "user") {
    return h(
      "div",
      { class: "chat-row user" },
      h("div", { class: "bubble", text: message.content }),
    );
  }
  return h("div", { class: "chat-row" }, h("div", { class: "reply", text: message.content }));
}

function typingDots(label?: string): HTMLElement {
  return h(
    "div",
    { class: "chat-row" },
    h(
      "div",
      { class: "typing" },
      h("i"),
      h("i"),
      h("i"),
      label
        ? h("span", { text: label, style: "margin-left:8px;font:400 11.5px var(--font);color:var(--dim-2)" })
        : null,
    ),
  );
}

/** The coloured chip showing what the question is about (a dropped file). */
function contextChip(label: string): HTMLElement {
  const chip = h("div", { class: "chip" }, h("i", { class: "chip-dot" }), h("span", { text: label }));
  requestAnimationFrame(() => chip.classList.add("settled"));
  return chip;
}

export function buildPrompt(onHeightChange: () => void): ViewHost {
  const chipRow = h("div", { class: "chip-row" });
  const log = h("div", { class: "chat-log" });
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Ask me anything…",
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Send" }, svg(ICONS.arrowUp, 11));
  // The microphone button: the same push-to-talk as the shortcut, for anyone who would
  // rather click. First press starts, second press stops.
  const mic = h("button", { class: "send-btn mic-btn", title: "Speak" }, svg(ICONS.mic, 12));
  const bar = h("div", { class: "chat-bar" }, input, mic, send);

  const el = h(
    "div",
    { class: "view" },
    h("div", { class: "card wash chat-card" }, h("div", { class: "chat-body" }, chipRow, log, bar)),
  );
  (el.querySelector(".card") as HTMLElement).style.setProperty("--wash", "rgba(99,102,241,0.5)");

  let sending = false;
  let renderedCount = -1;

  async function submit() {
    const query = input.value.trim();
    if (!query || sending) return;
    input.value = "";
    sending = true;
    Sound.play("send");

    State.chatHistory.push({ id: nextId++, role: "user", content: query });
    State.stateOverride = "thinking";
    State.notify();
    onHeightChange();

    const file = State.droppedFile;
    // The file rides along until a question has been answered. Counting messages
    // instead would lose it after a failed first attempt, whose bubble stays.
    const firstTurn = !State.chatHistory.some((m) => m.role === "assistant");
    const context: ChatContext | null =
      firstTurn && file ? { kind: "file", name: file.name, path: file.path } : null;

    try {
      const reply = await Bridge.chatSend(query, context);
      State.stateOverride = null;
      if (reply.cancelled) {
        // Stopped by the person. Rust dropped the unanswered message from the
        // conversation; the log drops it too.
        if (State.chatHistory.at(-1)?.role === "user") State.chatHistory.pop();
      } else {
        // A reply that is only a proposed action has no words; its card appears on
        // its own (the `assistant` event) and brings its own sound.
        if (reply.text) State.chatHistory.push({ id: nextId++, role: "assistant", content: reply.text });
        if (!reply.proposal) Sound.play("finish");
      }
    } catch (err) {
      State.stateOverride = null;
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      State.view = "note";
      Sound.play("error");
    } finally {
      sending = false;
      State.notify();
      onHeightChange();
      input.focus();
    }
  }

  submitText = (text) => {
    if (sending) {
      // A spoken request is never queued behind another and never replaces it (Rust
      // refuses a push while a request is running; this is for one typed meanwhile).
      // It is dropped, and the person is told, so nothing is lost without a word.
      State.noteMessage = "Coucou is still working on your last request.";
      State.view = "note";
      State.notify();
      return;
    }
    input.value = text;
    void submit();
  };

  mic.addEventListener("click", () => {
    if (State.voice.phase !== "idle") {
      void Bridge.voiceStop();
      return;
    }
    State.voice.viaButton = true;
    Bridge.voiceStart().catch((err) => {
      State.voice.viaButton = false;
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      State.view = "note";
      State.notify();
    });
  });

  // While a request is in flight the button is Stop: the person can always cancel.
  send.addEventListener("click", () => (sending ? void Bridge.assistantCancel() : void submit()));
  let showingStop = false;
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
      const file = State.droppedFile;
      const wantChip = file?.name ?? "";
      if (chipRow.dataset.label !== wantChip) {
        chipRow.dataset.label = wantChip;
        clear(chipRow);
        if (wantChip) chipRow.append(contextChip(wantChip));
      }

      const thinking = State.stateOverride === "thinking";
      // While the model's one look at the screen is being taken, the dots say so.
      const looking = thinking && State.assistant.phase === "capturing";
      const count = State.chatHistory.length + (thinking ? 0.5 : 0) + (looking ? 0.25 : 0);
      if (count !== renderedCount) {
        renderedCount = count;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        if (thinking) log.append(typingDots(looking ? "Looking at your screen…" : undefined));
        log.scrollTop = log.scrollHeight;
      }

      input.placeholder = State.chatHistory.length === 0 ? "Ask me anything…" : "Continue…";
      input.disabled = sending;
      const voiceOn = State.settings.voiceEnabled;
      mic.disabled = sending || !voiceOn;
      mic.title = voiceOn ? "Speak" : "Voice is off. Turn it on in Settings.";
      // Swapped only when it changes: rebuilding a button between a mouse-down and
      // a mouse-up would swallow the click.
      if (sending !== showingStop) {
        showingStop = sending;
        send.title = sending ? "Stop" : "Send";
        clear(send);
        send.append(svg(sending ? ICONS.xmark : ICONS.arrowUp, 11));
      }
    },
    focus() {
      input.focus();
      input.select();
    },
  };
}
