// Helpers that run.mjs and bench.mjs share for starting the servers under test on a free port and
// waiting until they listen on it.

import { execFileSync } from 'node:child_process';
import { createServer, connect } from 'node:net';

/** A TCP port on 127.0.0.1 that was free when this returned. */
export function freePort() {
  return new Promise((res, rej) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => res(port));
    });
    s.on('error', rej);
  });
}

/** Whether a TCP connection to `port` on 127.0.0.1 succeeds. */
export function canConnect(port) {
  return new Promise((res) => {
    const s = connect(port, '127.0.0.1');
    s.on('connect', () => {
      s.destroy();
      res(true);
    });
    s.on('error', () => res(false));
  });
}

/** The `[pid, ppid, rss]` rows of `ps` for `pid` and its descendants, with RSS in KiB. */
export function processTree(pid) {
  let out;
  try {
    out = execFileSync('ps', ['-A', '-o', 'pid=,ppid=,rss='], { encoding: 'utf8' });
  } catch {
    return [];
  }
  const rows = out
    .trim()
    .split('\n')
    .map((l) => l.trim().split(/\s+/).map(Number));
  const pids = new Set([pid]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const [p, pp] of rows) {
      if (pids.has(pp) && !pids.has(p)) {
        pids.add(p);
        grew = true;
      }
    }
  }
  return rows.filter(([p]) => pids.has(p));
}

/** The ids of the processes listening on TCP `port`, from `lsof`, or `null` without `lsof`. */
function listeners(port) {
  try {
    const out = execFileSync('lsof', ['-nP', `-iTCP:${port}`, '-sTCP:LISTEN', '-t'], { encoding: 'utf8' });
    return out.trim().split('\n').map(Number);
  } catch (e) {
    if (e.code === 'ENOENT') return null;
    // `lsof` exits with status 1 if nothing matches.
    if (e.status === 1) return [];
    throw e;
  }
}

/**
 * Waits until `child` or one of its descendants listens on `port` on 127.0.0.1, polling every
 * `pollMs`, and returns the `performance.now()` time of the first connection that succeeded. Throws
 * if `child` exits, if another process listens on the port, or after `timeoutMs`. Without `lsof`,
 * a connection while `child` is running counts as `child` listening.
 */
export async function waitForListener(child, port, { timeoutMs = 30000, pollMs = 50 } = {}) {
  const deadline = performance.now() + timeoutMs;
  while (performance.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`process exited with ${child.exitCode ?? child.signalCode}`);
    }
    if (await canConnect(port)) {
      const connectedAt = performance.now();
      const owners = listeners(port);
      const tree = new Set(processTree(child.pid).map(([pid]) => pid));
      if (owners === null || owners.some((pid) => tree.has(pid))) return connectedAt;
      if (owners.length > 0) throw new Error(`port ${port} is held by process ${owners.join(', ')}, not by the server`);
    }
    await new Promise((r) => setTimeout(r, pollMs));
  }
  throw new Error(`port ${port} not listening after ${timeoutMs} ms`);
}
