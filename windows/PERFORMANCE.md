# Idle resource use (Windows)

How much CPU, memory and how many wakeups Coucou costs when nothing is happening,
how that is measured, and what was found. The project rule is **0 % CPU when the
island is hidden**; this is how that rule is checked.

## What runs in the background

| Activity | Where | Frequency | While hidden |
|---|---|---|---|
| Cursor poll (click-through, hover, display-change check) | Rust thread, `island.rs` | every 16 ms | Parked on a condvar. The window shrinks to a 240×6 strip and the poll stops. |
| Frame loop | page, `island.ts` | `requestAnimationFrame` | Stops as soon as geometry has settled; never runs while hidden. |
| Pollers: n8n, Vercel, Stripe, Resend, GitHub, Cal.com, Notion | tokio tasks, `integrations.rs` | every 15 / 30 / 30 / 60 / 300 / 300 / 300 s | Wake, find the integration off (or Coucou paused) and go back to sleep; no network. |
| Relay pipe | tokio task, `pipe.rs` | none | Awaits a connection; wakes only for a hook. |
| UI timers (auto-hide, blink, greeting) | page | one-shot `setTimeout`s | None are pending. There is no `setInterval` and no file watcher. |
| Audio | page, `sound.ts` | none | The `AudioContext` is suspended 1.5 s after the last sound. |
| Endless CSS animations (drop frame, ticker shimmer, typing dots, pulse) | `style.css` | per frame | **Were running. Fixed, see below.** |
| Settings window | second hidden webview | none | 0 animations, 0 style recalcs. It exists at launch on purpose (a WebView2 window created later comes up blank, see `lib.rs`), which is part of the memory below. |

## Method

`scripts/measure-idle.ps1` launches a fresh copy of the release build with its own
`APPDATA` and `LOCALAPPDATA` (so it never reads your settings or collides with a
running Coucou), lets it settle, then samples the whole process tree for a fixed
time: `coucou.exe` and every WebView2 process it spawned.

- **CPU:** cumulative processor time of the tree from the OS performance counters
  (`Win32_PerfRawData_PerfProc_Process`) divided by wall time, as a percentage of
  one logical core. Counters update coarsely, so a window of 35 to 60 s is what means
  something; the per-sample buckets only show shape.
- **Memory:** sum of working set and private bytes over the tree.
- **Wakeups:** context switches per second, summed over the tree's threads.
- **A run is valid only if** the process count is one instance's (about nine), the
  island was in the state the scenario names (checked from the island window's size
  and, for compact/home, from an image of that window alone taken with
  `PrintWindow`, so nothing from the desktop is captured), and the mouse was never
  over the island's zone. Invalid runs are listed in the CSV but not averaged.
- Builds were compared in an **interleaved** order (before, after, per scenario) so
  background load on the machine affects both.

Scenarios: `startup` (first 40 s), `compact` (Mochi visible, no hover), `hidden`
(after the island hides itself), `home` (expanded panel left alone), `session` (a
Claude Code session: a prompt and a tool call every few seconds sent over the real
relay pipe).

```powershell
npm run tauri build -- --no-bundle
.\scripts\measure-idle.ps1 -Scenario hidden -Runs 3        # also: startup, compact, home, session
.\scripts\check-idle-render.ps1 -State hidden               # pass/fail: is the page quiet?
```

`check-idle-render.ps1` attaches to the island page over DevTools and fails if the
hidden or compact island has a running animation or recalculates styles more than a
handful of times a second. `cargo test` has the matching source-level guards
(`island::tests`).

**Test machine:** Intel Core i7-7500U (2 cores / 4 threads, 2.7 GHz), 15.9 GB RAM,
Windows 11 Pro 10.0.22000, WebView2 runtime 154.0.4258.48, release build with the
repository's profile (`opt-level = "s"`, LTO, stripped), built by
`tauri build --no-bundle`. Measured 2026-10-03. Percentages are of one logical core;
divide by four for the share of this machine.

## What was found

**Confirmed by measurement:** with the island hidden, the page still had two endless
SVG animations running (`dash-march` and `dash-breathe` on the drop card's dashed
frame). They animate on the main thread, so Chromium recalculated styles **60 times a
second** and kept the GPU process compositing, costing about 4 % of a core and 570
wakeups a second in a state that should cost nothing. The animations ran because every
view stays in the DOM and only the active one is made opaque; the compact and open
states paid the same price.

**Fix:** pause endless animations unless they are in the active view of an open
island (`.view:not(.on) *` and `#content:not(.shown) *` in `style.css`; `Island.syncDom`
sets `shown`). Paused, not removed, so nothing visible changes.

## Results

Valid runs only (`n` is the number of them). CPU in % of one core, mean (spread).

| Scenario | Before | After | Notes |
|---|---|---|---|
| hidden | 3.46 (n=1) | **0.05** (0.02, 0.07; n=2) | context switches 571/s → **2/s** |
| compact | 14.5 (24.0, 4.9; n=2) | **0.98** (0.66, 1.40, 0.89; n=3) | before is very noisy |
| home | 19.7 (13.4, 25.9; n=2) | 4.3 (6.8, 1.8; n=2) | still not zero, see below |
| session | 12.5 (20.1, 8.6, 8.9; n=3) | 7.9 (8.9, 6.8; n=2) | Mochi really is animating here |
| startup (40 s) | 23.1 (sd 1.3; n=3) | 15.1 (n=1) | includes the launch burst |

Many runs were rejected because the mouse reached the island's zone or the island
changed state mid-window, so the *before* hidden figure rests on one run. It agrees
with an independent per-process check of the same build (renderer 0.13 to 0.52 s of
CPU per 10 s plus the GPU process 0.05 to 0.2 s per 10 s, about 4 %), and the
mechanism is confirmed directly: the baseline page reports 2 running animations and
60 style recalcs/s hidden, the fixed one 0 and 0.2.

**Memory** is unchanged by the fix: about 460 to 490 MB working set and 200 to 230 MB
private across nine processes (about 150 to 170 threads, 3,700 handles), the same in
every state. A 10-minute hidden soak showed no growth (463 → 455 MB working set).
This is mostly WebView2: two renderers (island and the hidden settings window), the
GPU and network processes, and the browser process.

## Still open

- **The open panel is not free.** `home` still averages a few percent. Profiling its
  JavaScript shows the page is 98 % idle in script; the remainder is the browser
  presenting a visible 720×320 window while the island's frame loop keeps requesting
  frames. Reducing it further means changing how the panel renders, which was out of
  scope.
- **A working session animates by design** (`session` above); that is Mochi, not a
  leak.
- Linux was not measured, and the figures are from one machine. The page-level fix and
  its guards apply to the Linux build, which shares the same frontend.
- Memory (about 470 MB for a few pills) is the larger cost and is WebView2's baseline;
  the second, hidden webview is required by the blank-window issue noted above.
