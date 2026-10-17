// Nimo, as video. Two looping, transparent clips (VP9 with an alpha channel, which
// WebView2 plays) stand in for the drawn body while the mascot is only idling or while
// the microphone is confirmed live. Every other state keeps the engine's own drawing.
//
// One <video> per clip, never two for the same one, and only one ever plays: switching
// starts the new clip, waits until it has painted a frame, and only then hides and
// pauses the old one, so there is no empty frame, black box or flash in between. If a
// clip cannot load or play, `showing` stays false and the island keeps drawing the body
// it always drew; nothing else depends on the video.

import idleUrl from "../assets/mascots/nimo-idle.webm";
import listeningUrl from "../assets/mascots/nimo-listening.webm";
import { Bridge } from "../core/bridge";

export type NimoClip = "idle" | "listening";

interface Clip {
  video: HTMLVideoElement;
  failed: boolean;
}

function makeVideo(src: string): HTMLVideoElement {
  const v = document.createElement("video");
  v.className = "nimo-clip";
  v.muted = true; // visual only: never any sound
  v.defaultMuted = true;
  v.loop = true;
  v.playsInline = true;
  v.preload = "auto";
  v.disablePictureInPicture = true;
  v.tabIndex = -1;
  v.setAttribute("aria-hidden", "true");
  v.src = src;
  return v;
}

export class NimoMascot {
  readonly el: HTMLDivElement;
  /** Told when a clip starts or stops standing in for the body. */
  onChange: (() => void) | null = null;

  private clips: Record<NimoClip, Clip>;
  private wanted: NimoClip | null = null;
  /** Showing a frozen frame instead of playing: decoding costs CPU, a still costs none. */
  private held = false;
  private live: NimoClip | null = null;
  private token = 0;
  private placed = "";

  constructor() {
    this.el = document.createElement("div");
    this.el.id = "nimo";
    this.clips = {
      idle: { video: makeVideo(idleUrl), failed: false },
      listening: { video: makeVideo(listeningUrl), failed: false },
    };
    for (const name of ["idle", "listening"] as const) {
      const clip = this.clips[name];
      clip.video.addEventListener("error", () => this.fail(name));
      this.el.append(clip.video);
    }
  }

  /** A clip's frames are on screen, so the drawn body should step aside. */
  get showing(): boolean {
    return this.live !== null;
  }

  /**
   * Which clip should show, or none. With `hold` it shows as a still (paused on a
   * frame) rather than playing. Does nothing when it is what is already asked for.
   */
  show(want: NimoClip | null, hold = false) {
    if (want === this.wanted && hold === this.held) return;
    if (want !== null && want === this.wanted && this.live === want) {
      // The same clip, now to be frozen or let go again.
      this.held = hold;
      const { video } = this.clips[want];
      if (hold) video.pause();
      else void video.play().catch(() => this.fail(want));
      return;
    }
    this.wanted = want;
    this.held = hold;
    const token = ++this.token;

    if (want === null || this.clips[want].failed) {
      this.stopAll();
      return;
    }
    // The outgoing clip stops at once but stays on screen, frozen on its last frame,
    // until the new one has painted: never two playing, never an empty frame.
    for (const other of ["idle", "listening"] as const) {
      if (other !== want) this.clips[other].video.pause();
    }
    const { video } = this.clips[want];
    try {
      video.currentTime = 0;
    } catch {
      // Not loaded yet: it starts from the beginning anyway.
    }
    video.play().then(
      () => {
        if (token !== this.token) return;
        // Only once a frame has actually been painted is the other clip let go.
        if ("requestVideoFrameCallback" in video) {
          video.requestVideoFrameCallback(() => token === this.token && this.reveal(want));
        } else {
          this.reveal(want);
        }
      },
      () => {
        if (token === this.token) this.fail(want);
      },
    );
  }

  /** Puts the clip's square, centred on (cx, cy), `side` px across. */
  place(cx: number, cy: number, side: number) {
    const key = `${cx.toFixed(1)},${cy.toFixed(1)},${side.toFixed(1)}`;
    if (key === this.placed) return;
    this.placed = key;
    const s = this.el.style;
    s.width = s.height = `${side.toFixed(1)}px`;
    s.left = `${(cx - side / 2).toFixed(1)}px`;
    s.top = `${(cy - side / 2).toFixed(1)}px`;
  }

  private reveal(name: NimoClip) {
    if (this.held) this.clips[name].video.pause();
    for (const other of ["idle", "listening"] as const) {
      const { video } = this.clips[other];
      if (other === name) {
        video.style.opacity = "1";
      } else {
        video.style.opacity = "0";
        video.pause();
      }
    }
    this.setLive(name);
  }

  private stopAll() {
    for (const { video } of Object.values(this.clips)) {
      video.style.opacity = "0";
      video.pause();
    }
    this.setLive(null);
  }

  private fail(name: NimoClip) {
    if (this.clips[name].failed) return;
    this.clips[name].failed = true;
    // Only the fact, never a path or anything from the page.
    void Bridge.log(`mascot: the ${name} clip could not be played; using the drawn mascot`);
    if (this.wanted === name) {
      this.stopAll();
    }
  }

  private setLive(next: NimoClip | null) {
    if (this.live === next) return;
    this.live = next;
    this.onChange?.();
  }
}
