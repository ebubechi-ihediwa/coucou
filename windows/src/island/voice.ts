// What Rust tells the island about push-to-talk (the `voice` event): the microphone is
// on, it is off and being transcribed, here is what was said, or why nothing came of
// it. The page shows that and nothing more. What was said goes to the one place a
// typed message goes (the chat's own submit), so the assistant, the policy and the
// approval card treat it exactly like typing; asking and answering are not decided here.

import { onEvent, type VoiceEvent } from "../core/bridge";
import { Sound } from "../core/sound";
import { State } from "../core/state";
import { submitVoiceQuery } from "../views/chat";
import type { Island } from "./island";

export function registerVoiceHandlers(island: Island) {
  /** Back to rest: nothing is recording, and nothing is held open. */
  const rest = () => {
    State.voice = { phase: "idle", note: null, viaButton: false };
    State.stateOverride = null;
    State.isPinned = false;
    island.dropPin();
  };

  void onEvent<VoiceEvent>("voice", (event) => {
    // Paused means paused: nothing wakes the island, whatever the shortcut did.
    if (State.paused && event.phase !== "idle") return;

    switch (event.phase) {
      case "listening":
        State.voice = { phase: "listening", note: null, viaButton: State.voice.viaButton };
        State.stateOverride = "working";
        // The card must not slip away while the microphone is on.
        State.isPinned = true;
        Sound.play("send");
        island.alert("voice");
        break;

      case "transcribing":
        State.voice = {
          phase: "transcribing",
          note: event.limitReached ? "The maximum recording length was reached." : null,
          viaButton: State.voice.viaButton,
        };
        State.stateOverride = "thinking";
        Sound.play("blip");
        State.notify();
        break;

      case "transcript":
        rest();
        // The same door a typed message uses.
        island.setView("prompt");
        submitVoiceQuery(event.text);
        break;

      case "notice":
      case "error":
        rest();
        State.noteMessage = event.message;
        Sound.play(event.phase === "error" ? "error" : "blip");
        island.alert("note");
        break;

      case "idle":
        // Cancelled. Nothing was said to anyone.
        rest();
        if (State.view === "voice") island.setView(State.defaultView());
        State.notify();
        break;
    }
  });
}
