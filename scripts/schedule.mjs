import { spawn } from 'node:child_process';
import { mkdir, open, readFile, unlink, writeFile, access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
const runtime = path.join(root, '.runtime');
const thread = '01a081c3-86d3-7f72-b1c5-4bea015f9901';
const start = Date.parse('2026-09-09T03:23:00+09:00');
const deadline = Date.parse('2026-09-12T23:59:00+09:00');
const interval = 301 * 60_000;
const stamp = value => new Date(value).toLocaleString('sv-SE', { timeZone: 'Asia/Tokyo' });
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const exists = filename => access(filename).then(() => true, () => false);

export function scheduledTimes(now = Date.now()) {
  const values = [];
  for (let tick = start; tick <= deadline; tick += interval) if (tick >= now) values.push(tick);
  return values;
}

async function daemon() {
  await mkdir(runtime, { recursive: true });
  const lockfile = path.join(runtime, 'scheduler.lock');
  const lock = await open(lockfile, 'wx').catch(() => null);
  if (!lock) throw new Error('Scheduler lock exists. Inspect .runtime/scheduler.lock before restarting.');
  await lock.writeFile(JSON.stringify({ pid: process.pid, thread, start, deadline }));
  await lock.close();
  try {
    const statePath = path.join(runtime, 'scheduler-state.json');
    const previous = await readFile(statePath, 'utf8').then(JSON.parse, () => ({}));
    // On wake after sleep, deliver one outstanding tick, not a burst of all missed ticks.
    let tick = Math.max(start, (previous.lastTick ?? (start - interval)) + interval);
    while (Date.now() <= deadline && tick <= deadline) {
      if (await exists(path.join(runtime, 'scheduler.stop')) || await exists(path.join(root, 'docs/delivery-complete.json'))) break;
      if (Date.now() < tick) { await sleep(Math.min(30_000, tick - Date.now())); continue; }
      const message = `Scheduled continuation for Kurabase (${stamp(tick)} Asia/Tokyo). Continue the user's authorized development now. Read docs/progress.md and docs/decisions.md, inspect current work and live agents, preserve all changes. Coordinate Terra for bounded SDK/UI/docs work and Astra for SQL/contracts/security. Complete chain-backed implementation, independent JS SDK, dashboard, reference/LLM docs, and meaningful tests by 2026-09-12 23:59 JST. Record verified progress and remaining gaps. Do not start a second scheduler. If quota blocks work, save checkpoint; the next tick is 301 minutes later. Stop scheduling only after verified delivery by writing docs/delivery-complete.json.`;
      console.log(`${new Date().toISOString()} queuing tick ${stamp(tick)}`);
      const exitCode = await new Promise(resolve => {
        // Queue onto the exact existing thread: serializes with active work and avoids two independent writers.
        const child = spawn('codex', ['queue', '-C', root, '--thread', thread, '--message', message], { cwd: root, stdio: 'inherit' });
        child.once('error', error => { console.error(error.message); resolve(1); });
        child.once('exit', code => resolve(code ?? 1));
      });
      await writeFile(statePath, JSON.stringify({ lastTick: tick, queuedAt: new Date().toISOString(), exitCode, nextTick: stamp(tick + interval) }, null, 2));
      console.log(`queue exit=${exitCode}`);
      tick += interval;
      while (tick < Date.now()) tick += interval;
    }
  } finally { await unlink(lockfile).catch(() => {}); }
}

if (process.argv.includes('--dry-run')) {
  console.log(JSON.stringify({ thread, timezone: 'Asia/Tokyo', intervalMinutes: 301, deadline: stamp(deadline), ticks: scheduledTimes().map(stamp) }, null, 2));
} else if (process.argv.includes('--start')) {
  await mkdir(runtime, { recursive: true });
  if (await exists(path.join(runtime, 'scheduler.lock'))) throw new Error('Scheduler already has a lock.');
  const log = await open(path.join(runtime, 'scheduler.log'), 'a');
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), '--daemon'], { cwd: root, detached: true, stdio: ['ignore', log.fd, log.fd] });
  child.unref();
  await log.close();
  console.log(`Started Kurabase scheduler PID ${child.pid}; log .runtime/scheduler.log`);
} else if (process.argv.includes('--daemon')) {
  await daemon();
} else {
  console.log('Usage: node scripts/schedule.mjs --dry-run | --start | --daemon');
}
