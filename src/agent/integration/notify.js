import { openSync, writeSync, closeSync, constants } from "node:fs";
import tty from "node:tty";

const SESSION_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/;
const TOKEN_RE = /^[A-Za-z0-9_-]{1,128}$/;

function sanitize(value) {
  return String(value ?? "").replace(/[\x00-\x1f\x7f-\x9f;]/g, "");
}

// Hook stdout belongs to the agent's protocol. Write to the PTY device Kumu
// injected; agent hooks may have no controlling terminal to fall back on.
export function notifySession(agent, session) {
  // Session ids travel as a notification-shaped OSC; Kumu records them
  // instead of showing them. Reject malformed ids rather than rewriting them.
  const id = String(session ?? "");
  if (!SESSION_ID.test(id) || id.includes("..")) return;
  const token = process.env.KUMU_SESSION_TOKEN || "";
  if (token && TOKEN_RE.test(token)) {
    notify(agent, `kumu-session:${token}:${id}`);
  } else {
    notify(agent, `kumu-session:${id}`);
  }
}

export function notify(agent, message) {
  if (process.env.KUMU_TERMINAL !== "1") return;
  const cleanAgent = sanitize(agent);
  const cleanMessage = sanitize(message);
  if (!cleanAgent || !cleanMessage) return;
  const path = process.env.KUMU_TTY || "/dev/tty";
  if (!path.startsWith("/dev/")) return;
  const flags =
    (constants.O_WRONLY || 0) |
    (constants.O_NOCTTY || 0) |
    (constants.O_NONBLOCK || 0);
  let fd;
  try {
    fd = openSync(path, flags);
    if (!tty.isatty(fd)) return;
    writeSync(fd, `\x1b]777;notify;${cleanAgent};${cleanMessage}\x07`);
  } catch {
    // The PTY may already be closed, or the agent runs headless.
  } finally {
    if (fd !== undefined) {
      try {
        closeSync(fd);
      } catch {
        // Ignore close errors so we never crash the host agent.
      }
    }
  }
}
