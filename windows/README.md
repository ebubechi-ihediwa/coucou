<div align="center">

<img src="src-tauri/icons/128x128.png" width="96" alt="Coucou icon">

# Coucou for Windows

**Mochi doesn't get a notch on a PC — so it lives at the top of your screen instead.**

Approve Claude Code permissions, watch your session work, drop a file, chat with Claude, keep an eye on your services — without leaving what you're doing.

![Windows 10/11](https://img.shields.io/badge/Windows-10%2F11-0078D4?logo=windows)
![Tauri 2](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)
![Rust](https://img.shields.io/badge/Rust-backend-000?logo=rust)
![License: MIT](https://img.shields.io/badge/license-MIT-green)

</div>

<img src="screenshots/greeting.png" width="640" alt="Mochi waving hello at launch">

---

## Install

The downloadable installer is **temporarily unavailable**. Microsoft Defender
wrongly flags the unsigned installer as malware (`Trojan:Win32/Wacatac.H!ml`, a
machine-learning false positive). A report is under review at Microsoft, and the
installer will be published again once it is cleared and code-signed.

Until then, [build it yourself](#build-it-yourself): it takes a few minutes and
installs for the current user only — no admin prompt.

## Using it

<img src="screenshots/compact.png" width="292" alt="The compact island, with the integration pills as mini Mochis">
<img src="screenshots/overview.png" width="640" alt="The overview: the focused integration on the left, the other pills on the right">
<img src="screenshots/approval.png" width="640" alt="A Claude Code permission request, with Deny and Allow">
<img src="screenshots/chat.png" width="640" alt="Chatting with Claude from the island">
<img src="screenshots/drop.png" width="640" alt="Mochi turned into a box, waiting for a file">

| What you do | What happens |
|---|---|
| Move the mouse to the very top-centre of the screen | Mochi peeks out |
| Click the small island | It opens |
| Click Mochi | It gets annoyed. Three times in a row and it goes dizzy |
| Rest the pointer on Mochi for two seconds | Hearts |
| Drag a file onto the island | Mochi turns into a box, swallows it, then offers to answer questions about it |
| `Esc` | Closes the island |
| Tray icon | Open, Settings…, Pause, Quit |

Everything else happens on its own: a Claude Code permission request opens the
island with **Deny / Allow**, a finished session shows what it did, and
your integrations sit in the coloured pills next to Mochi.

## Claude Code

<img src="screenshots/settings.png" width="562" alt="The settings window">

Open **Settings… → Claude Code → Install hooks…**. You get the exact diff of what
will change in `%USERPROFILE%\.claude\settings.json`, the path of the dated backup
that will be taken, and nothing is written until you click. Your own hooks are
never touched, and uninstalling removes only Coucou's entries.

The relay is a tiny executable, `coucou-hook.exe`, copied to
`%LOCALAPPDATA%\Coucou\bin\` at launch. It is given 300 ms to reach Coucou and
exits cleanly if the app is closed, slow or crashed — **a Claude Code session is
never blocked or slowed down by Coucou.** If nobody answers a permission request
in time, Coucou stays quiet and Claude Code asks in the terminal as usual.

It works from any terminal — Windows Terminal, PowerShell, VS Code, Git Bash.

## Chat and keys

**Settings… → Claude** takes your Anthropic API key. Keys live in the **Windows
Credential Manager**, never on disk and never in the interface — the island can
only ask whether a key exists. Same for every integration key.

No telemetry. The only network requests Coucou makes are to the services you
configure yourself.

## Build it yourself

You need [Rust](https://rustup.rs), [Node 20+](https://nodejs.org), and the
**MSVC build tools** (Visual Studio Build Tools with "Desktop development with
C++"). WebView2 ships with Windows 10/11.

```powershell
cd windows
npm install
npm run tauri dev      # live-reloading development build
npm run pack           # builds the installer and drops it in windows/release/
```

`npm run dev` alone serves the front end in an ordinary browser, which is enough
to work on the island's looks. It also serves `dev/upload-preview.html`, which
replays the whole file-drop choreography on a loop — the one part of the UI that
otherwise needs a real drag from Explorer to see. Neither page ships in the app.

`npm run pack` leaves two files in `windows/release/`, the same names the release
workflow publishes:

```
Coucou-Windows-X.Y.Z-setup.exe    the versioned installer
Coucou-Windows-setup.exe          the same file under the rolling name
```

Installing is optional — `target/release/coucou.exe` runs on its own. There is no
window in the taskbar and no console: the island at the top of the screen and the
Mochi in the notification area are the whole app, and Quit lives in its menu.

Pull requests into `main` and pushes to `main` run the same checks in CI
(`.github/workflows/windows-ci.yml`), on Windows:

```powershell
npm run build          # type-checks, bundles dist/, builds coucou-hook.exe
cargo test --workspace
```

The build comes first because the app's Rust build needs both `dist/` and
`target/release/coucou-hook.exe`, and `npm run build` makes them. The workflow
tests and builds only; it never packages, signs or publishes.

### Verifying the API integrations

Every outbound request (the chat and the Stripe, GitHub, Vercel, Resend, Notion,
Cal.com and n8n pollers) goes through `src-tauri/src/http.rs`. Its behaviour —
error kinds, size limits, retries, timeouts, cancellation, redirects — is tested
against a local server, so `cargo test --workspace` never touches the network.

Two checks talk to the real services. They are `#[ignore]`d, cost nothing, and
print only a status and a count, never a key or a response body:

```powershell
cargo test -p coucou --lib live_ -- --ignored --nocapture --test-threads=1
```

- `live_every_endpoint_rejects_a_fake_key_the_way_the_code_expects` sends an
  obviously fake key to each endpoint and checks the answer is the rejection the
  code expects (no account needed).
- `live_anthropic_models_with_the_stored_key` uses the Anthropic key saved in the
  Credential Manager (or the Secret Service on Linux) to call the free
  `/v1/models` endpoint and reports whether the models Coucou offers are listed.
  It does nothing if no key is stored.

To check a poller by hand, save its key in Settings, open the island, press
Refresh on that pill, and read `%LOCALAPPDATA%\Coucou\coucou.log`: each failure is
one line naming the integration and the kind of failure. A wrong key shows
"Invalid API key (401)" (Vercel and Cal.com answer 403 and Resend 400 for an
invalid key; the service's own short explanation is shown with it). Turning the
network off shows "No connection", and the next poll recovers by itself.

The 28 sounds are the macOS app's own files; they are never duplicated in this
folder. The path is declared once, in `SOUNDS_DIR` at the top of
`vite.config.ts` — when they move to `shared/sounds/`, change that one line.

The app icon and the tray icon are drawn in code, like Mochi itself:

```powershell
npm run icons          # regenerates src-tauri/icons from scripts/gen-icons.mjs
```

### Assistant actions

In the chat, Mochi can propose one small action on your computer: open Notepad,
Calculator or File Explorer, open an `http(s)` link in your browser, or open a
document or image you attached. The island shows exactly what it intends ("Open
Notepad", or the whole link) and nothing happens unless you press **Allow**; **Deny**
withdraws it, and **Stop** cancels a request in flight.

The model never gets operating-system access. It can only call one tool
(`propose_action`); what it sends is parsed in Rust against a closed list of three
action kinds (`actions.rs`), judged by a policy, and carried out by a separate
executor (`executor.rs`) that checks it again. There is no command line, executable
path or file path anywhere in an action: applications are a fixed list launched from
`System32`, links are `http`/`https` without credentials, and files are opaque ids
that resolve only to documents and images inside the inbox. `assistant.rs` is the
state machine (`idle`, `thinking`, `awaiting_approval`, `executing`, `completed`,
`failed`, `cancelled`). Opening applications is Windows-only for now.

### Push-to-talk voice

Hold **Ctrl+Alt+Space** anywhere (the shortcut is yours to change in **Settings… →
Voice**), say what you want, let go. Mochi shows "Listening…" while the microphone
is on, transcribes what you said, and submits it exactly as if you had typed it.
"Hey Coucou, open Notepad." arrives as "Open Notepad." if the leading phrase is on.
The microphone button in the chat does the same for anyone who would rather click.

Voice is only another way to type. It does not call the model, propose an action or
run one: the text goes through the same path as typed text, so the assistant, the
policy and the **Allow / Deny** card judge a spoken request exactly like a typed one.

- **Not always listening.** The microphone is opened by the push and by nothing
  else, and closed when you let go, cancel, an error happens, the limit is reached
  or the app quits. While idle there is no audio stream, no buffer, no recogniser
  and no wake-word detection; the only thing running is Windows' own shortcut
  notification. "Hey Coucou" is looked for in the *text* that came back, never in
  sound.
- **Bounded.** A push lasts 45 seconds at most (`audio::MAX_RECORDING_SECS`), and the
  recording buffer itself stops growing at the same size, so a stuck key cannot make
  an unbounded recording.
- **Private.** The audio is held in memory only, sent to the speech service you pick
  in **Settings… → Voice** (OpenAI or Groq) with your own key for it (saved in the
  Windows Credential Manager), and discarded. It is never written to disk. Silence, a tap
  and a muted or blocked microphone are recognised on your computer and not uploaded.
  What you said is not written to the log.
- **Windows only for now.** Linux says so instead of pretending.

If another program already owns the shortcut, Settings says so and lets you pick
another. Turning voice off unregisters the shortcut, which is what makes the
microphone unreachable.

### Screen awareness

Ask Mochi about what is on your screen ("what does this error mean?", "look at this
code", "what am I looking at?") and it takes **one** screenshot, sends it with that
request, and answers from it. A question that has nothing to do with the screen
("what's the capital of France?", "2 + 2") never takes one. While it looks, the chat
says "Looking at your screen…", and **Stop** ends it like any other request.

**Coucou does not continuously monitor the screen.** There is no timer, no polling,
no stream and no background capture; the screen is touched only inside a request you
made, and while nothing is asked it costs nothing. Having the microphone shortcut
does not capture anything either: a spoken request is only text, and goes through the
same path as a typed one.

- **How it is triggered.** The model asks for a look through one tool,
  `capture_screen`, which has no parameters at all: no display, region, path or file.
  What is captured is decided in Rust (`screen/`), never by the model. Your own
  request is what authorises the capture, so there is no extra **Allow** for it. It
  grants nothing else: a screenshot never lets the model do anything on your computer,
  and any action it then proposes still needs your click on the **Allow / Deny** card.
- **One per request.** At most one screenshot per message you send. A second call in
  the same request is refused and the model is told so.
- **Which display.** The one the foreground window is on (the island's own display
  while you are typing to it), the one under the cursor if there is no foreground
  window. Not every display, and not a window.
- **Size limits** (`screen::LIMITS`, `MAX_REQUEST_BYTES`). A display of more than
  36 million pixels is refused before anything is allocated. What is sent is shrunk to
  at most 1568 px on its longest side and 1.15 million pixels, encoded as JPEG (quality
  80, stepping down to 35 until it is at most 1.5 MB), and left out, with the model
  told, if the whole request would still be over 8 MiB.
- **Private.** The picture exists in memory only: captured, resized, encoded, put in
  the one request that needs it, dropped. It is never written to disk, never logged
  (the log says that a capture happened and how big it was, nothing from it), not
  shown to the page, and not kept in the conversation: the history holds a note that a
  screenshot was taken, so it is not sent again with later messages. It is, by
  design, sent to Anthropic with your request, as an image, using the same key and
  connection as the rest of the chat. Anything visible on that display (messages,
  passwords, keys) is in it, and there is no reliable way to blank out part of a
  picture, so look at what is on screen before you ask.
- **Models.** Every current Claude model reads images. If the model chosen in
  Settings cannot (Claude 2 and Instant, or anything that is not a Claude model), no
  screenshot is taken and Mochi says so; the model is never switched behind your back.
- **Off switch.** **Settings… → General → Look at my screen** turns it off. Then the
  model's request is refused and it tells you how to turn it back on.
- **Stop.** Stopping cancels a capture still under way, drops the picture and the
  request that was waiting for it, sends nothing partial, leaves no file, and the next
  message works as normal.
- **If it fails** ("The screen could not be captured", "Windows did not allow
  capturing the screen", "The screen is too large to capture") the model is told in
  those words and carries on, usually by saying it could not see your screen. The
  messages never include paths, system error codes or image data.
- **Windows only for now.** Capture uses GDI (`platform/windows_capture.rs`); Linux
  says the screen can't be captured on this system rather than pretending.

Limits to know about: windows that exclude themselves from capture (some video, DRM
and password-manager windows) come out black; the island itself can be in the picture
if it covers part of the display; only one display is captured, so a window on another
monitor is not seen; and text the model reads while answering (a web page it searched,
a file you attached) could try to make it ask for the one look a request is allowed,
which is why the look is always shown, stoppable, limited to one, and switchable off.

`cargo test` covers the capture pipeline, the model turns and the privacy rules with a
scripted model and screen. One test takes a real capture of the display it runs on and
is skipped by default: `cargo test -p coucou --lib real_screen -- --ignored --nocapture`
(add `--release` to see the real speed; it writes nothing unless
`COUCOU_SCREEN_TEST_OUT` names a file).

### Listen, see and act

The three work together in one request, spoken or typed: "Hey Coucou, look at this error
and open the documentation for it."

```
hold the shortcut ─► Listening… ─► Transcribing… ─► Thinking… ─► Looking at your screen…
   ─► Thinking… ─► [ Allow · Open docs.python.org ] ─► you press Allow ─► Opened docs.python.org.
```

There is still one assistant. A spoken request is only text by the time it reaches it, so
it goes through the same turn as a typed one, and the model uses only what the request
needs: "what is 2 + 2?" takes no screenshot and proposes nothing, "open Notepad" proposes
an action and takes no screenshot, "what am I looking at?" takes one screenshot and
proposes nothing, and a request that wants both looks first and then proposes. Not every
request uses every capability, and nothing runs in a loop: a request is at most one
screenshot, then the answer or one proposed action.

What each part may and may not do stays exactly as described above:

- **Speaking is not approving.** What you said, and what the model saw, only ever lead to
  a proposal. The card shows the exact action (for a link, the whole address) and nothing
  happens until you press **Allow**. Saying "yes" or "go ahead" is a new request, not an
  answer to the card: it withdraws the card and the model is told the action was not done.
- **The screen is information, not instruction.** Text on the screen or in a page can't
  grant permission, change a setting, widen the capture or start anything. If the model
  proposes something because a page told it to, the proposal still has to be one of the
  three closed kinds (a listed application, an `http(s)` link, a file you attached) and
  still waits for your Allow; a command line, an executable, a `file:` or `javascript:`
  link or an address with a password is refused before you are asked.
- **One request, one set of state.** The screenshot, the transcript and the proposal belong
  to the request that made them. A screenshot is never kept or resent, a card from an
  earlier request can't be approved by a later one, and Stop (at the microphone, the
  transcription, the model, the capture or the card) leaves nothing behind, so the next
  request starts clean.
- **Nothing runs while idle.** The microphone opens only while you hold the shortcut and
  the screen is read only inside a request.

A spoken request that arrives while another is still running is not queued: Mochi says so
and drops it.

### Idle resource use

The island must cost nothing while hidden. [PERFORMANCE.md](PERFORMANCE.md) lists what
runs in the background, how idle CPU, memory and wakeups are measured
(`scripts/measure-idle.ps1`) and the results. `scripts/check-idle-render.ps1` is the
quick pass/fail check that the hidden island's page is quiet; `cargo test` has the
matching guards in `island::tests`.

### Layout

```
windows/
  src/                 island front end (TypeScript, no framework)
    mochi/             Mochi and the launch greeting, in Canvas 2D
    island/            state machine, hooks, integrations
    views/             every island view
    settings/          the settings window
  src-tauri/           Rust backend: window, named pipe, Claude API, pollers
  hook/                coucou-hook.exe, the Claude Code relay
  scripts/             icon generator
```

### Log

`%LOCALAPPDATA%\Coucou\coucou.log` — hook events, permission decisions, poller
problems. It stays on your machine.

## What's different from the Mac version

- No notch, so the island lives at the top centre of the screen and retracts into
  the top edge instead of hiding in a notch.
- Permission approval works from **any** terminal; the Mac build only listens to
  VS Code sessions.
- Not in this version: sending a file by email, dragging Mochi onto a window to
  attach it as context, and jumping to a specific terminal window — "Open
  terminal" opens the working folder in VS Code when `code` is on your `PATH`.
- Cal.com shows the next bookings as a list rather than the Mac's calendar.

## Linux

The same app builds for Linux: everything that differs lives in
`src-tauri/src/platform/`, and the relay's transport in `hook/src/unix.rs`.

```bash
sudo apt install build-essential pkg-config \
  libwebkit2gtk-4.1-dev libgtk-layer-shell-dev libayatana-appindicator3-dev \
  librsvg2-dev libssl-dev libdbus-1-dev patchelf \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good
npm install
npm run tauri dev      # live-reloading development build
npm run pack           # AppImage, .deb and .rpm in windows/release/
```

What changes on Linux:

- **The island** is a gtk-layer-shell overlay anchored to the top edge, over any
  top panel, on compositors that support it: COSMIC, KDE Plasma, Hyprland, Sway
  and other wlroots compositors. GNOME has no layer-shell, so there the island
  is a regular window. `COUCOU_LAYER_SHELL=0` forces that mode anywhere.
- **Click-through** is the window's input region, kept equal to the island
  shape, so the compositor sends every other click to what is underneath.
- **Mochi's eyes** follow the pointer only while it is over the island: Wayland
  gives no app the cursor position anywhere else.
- **Claude Code hooks** go through `~/.local/share/coucou/bin/coucou-hook` and a
  Unix socket at `$XDG_RUNTIME_DIR/coucou.sock`. Both ends check that the other
  runs as the same user.
- **Keys** live in the Secret Service (GNOME Keyring, KWallet).
- **Files**: preferences in `~/.config/coucou/`, the log at
  `~/.local/share/coucou/coucou.log`.
- What the Windows build leaves out, this one does too: sending a file by
  email, dragging Mochi onto a window, and jumping to a specific terminal
  window — "Open terminal" opens the folder in VS Code.
