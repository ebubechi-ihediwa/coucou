// Asks the island page of a running Coucou whether it is quiet, over DevTools.
// Used by check-idle-render.ps1; it only reads (animations, Chromium's own
// main-thread counters) and uses a timer, not requestAnimationFrame, because asking
// for frames would itself make the page render at 60 Hz.
//
//   node check-idle-render.mjs <devtools-port> <seconds> <hidden|compact|home>
//
// hidden / compact: no animation may be running and the page may recalculate styles
// at most a handful of times a second. home: reported, not asserted (the open
// island has legitimate motion while its countdown runs).

const port = process.argv[2] || "9223";
const seconds = Number(process.argv[3] || 6);
const state = process.argv[4] || "hidden";
const MAX_RECALCS_PER_SECOND = 5;

const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
const island = targets.find((t) => t.type === "page" && !t.url.includes("settings"));
if (!island) {
  console.error("no island page found on the DevTools port");
  process.exit(2);
}

const ws = new WebSocket(island.webSocketDebuggerUrl);
let nextId = 0;
const waiting = new Map();
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); }
};
await new Promise((resolve) => (ws.onopen = resolve));
const send = (method, params = {}) =>
  new Promise((resolve) => { const id = ++nextId; waiting.set(id, resolve); ws.send(JSON.stringify({ id, method, params })); });
const metrics = async () =>
  Object.fromEntries((await send("Performance.getMetrics")).result.metrics.map((m) => [m.name, m.value]));

await send("Performance.enable");
const before = await metrics();
const started = Date.now();
const probe = await send("Runtime.evaluate", {
  awaitPromise: true,
  returnByValue: true,
  expression: `new Promise((resolve) => setTimeout(() => resolve(
    document.getAnimations().filter((a) => a.playState === "running").map((a) => {
      const t = a.effect && a.effect.target;
      return (a.animationName || a.transitionProperty || "animation") + " on " + (t ? t.tagName : "?");
    })), ${seconds * 1000}))`,
});
const elapsed = (Date.now() - started) / 1000;
const after = await metrics();
const running = probe.result.result.value;
const recalcs = (after.RecalcStyleCount - before.RecalcStyleCount) / elapsed;
const taskMs = ((after.TaskDuration - before.TaskDuration) / elapsed) * 1000;

console.log(`state=${state}: ${running.length} running animation(s)${running.length ? " [" + running.join(", ") + "]" : ""}; ` +
  `${recalcs.toFixed(1)} style recalcs/s; ${taskMs.toFixed(1)} ms of main-thread tasks/s`);

if (state !== "home" && (running.length > 0 || recalcs > MAX_RECALCS_PER_SECOND)) {
  console.error(`FAIL: the ${state} island should be quiet (no running animations, at most ${MAX_RECALCS_PER_SECOND} style recalcs/s)`);
  process.exit(1);
}
console.log("ok");
process.exit(0);
