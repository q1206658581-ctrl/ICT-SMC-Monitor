// Forwards every console.log / warn / error / debug call to the Rust
// backend, which appends it to /tmp/ict-radar-ui.log. This gives the
// dev a single grep-able log file covering both halves of the app
// during M3 debugging (no more copy-pasting devtools output).
import { invoke } from '@tauri-apps/api/core';

let installed = false;
export function installConsoleForwarder() {
  if (installed) return;
  installed = true;
  const orig = {
    log: console.log.bind(console),
    info: console.info.bind(console),
    warn: console.warn.bind(console),
    error: console.error.bind(console),
    debug: console.debug.bind(console),
  };
  function send(level: string, args: unknown[]) {
    try {
      const safe = args.map((a) => {
        if (typeof a === 'string') return a;
        try { return JSON.stringify(a); } catch { return String(a); }
      });
      // Fire-and-forget; never throw out of the patched console.
      invoke('ui_log', { level, args: safe }).catch(() => {});
    } catch { /* ignore */ }
  }
  console.log = (...a: unknown[]) => { orig.log(...a); send('log', a); };
  console.info = (...a: unknown[]) => { orig.info(...a); send('info', a); };
  console.warn = (...a: unknown[]) => { orig.warn(...a); send('warn', a); };
  console.error = (...a: unknown[]) => { orig.error(...a); send('error', a); };
  console.debug = (...a: unknown[]) => { orig.debug(...a); send('debug', a); };
  // Capture uncaught errors / rejections too so they end up in the file.
  window.addEventListener('error', (ev) => {
    send('error', ['window.onerror', ev.message, ev.filename, ev.lineno, ev.colno]);
  });
  window.addEventListener('unhandledrejection', (ev) => {
    send('error', ['unhandledrejection', String(ev.reason)]);
  });
}
