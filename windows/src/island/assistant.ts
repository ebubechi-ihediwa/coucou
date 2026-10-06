// What Rust tells the island about the assistant (the `assistant` event): a proposed
// action to show, how it went, or that it was cancelled. The page keeps no logic of
// its own about any of it. It displays the snapshot and asks Rust (approve, deny,
// cancel) through the bridge.

import { onEvent, type AssistantSnapshot } from "../core/bridge";
import { Sound } from "../core/sound";
import { State } from "../core/state";
import type { Island } from "./island";

/** How long the result stays on the card before it hands the view back. */
const RESULT_MS = 2600;

let noteId = -1;

export function registerAssistantHandlers(island: Island) {
  let settleTimer: number | null = null;

  const clearTimer = () => {
    if (settleTimer != null) window.clearTimeout(settleTimer);
    settleTimer = null;
  };

  /** Lets the island close on its own again once nothing waits for an answer. */
  const unpin = () => {
    State.isPinned = false;
    island.dropPin();
  };

  void onEvent<AssistantSnapshot>("assistant", (snapshot) => {
    const before = State.assistant.phase;
    State.assistant = snapshot;
    clearTimer();

    switch (snapshot.phase) {
      case "awaiting_approval":
        // A card nobody has answered must not slip away: pinned, like a permission
        // request, until it is allowed, denied or cancelled.
        State.isPinned = true;
        Sound.play("approval");
        island.alert("action");
        break;

      case "completed":
      case "failed":
      case "cancelled": {
        unpin();
        // How an approved action went: say it in the chat as well, once. (A refused
        // proposal is already explained in the reply itself, and a cancel needs no
        // words.)
        if (before === "executing" && snapshot.message && snapshot.phase !== "cancelled") {
          State.chatHistory.push({ id: noteId--, role: "assistant", content: snapshot.message });
        }
        Sound.play(snapshot.phase === "completed" ? "finish" : "blip");
        if (State.view === "action") {
          settleTimer = window.setTimeout(() => {
            settleTimer = null;
            if (State.view === "action") island.setView(State.defaultView());
          }, RESULT_MS);
        }
        break;
      }

      case "idle":
      case "thinking":
      case "executing":
        break;

      case "capturing":
        // Part of the request, shown by the chat as "Looking at your screen…". Nothing to
        // answer and nothing to pin: the island is already open on the chat.
        break;
    }
    State.notify();
  });
}
