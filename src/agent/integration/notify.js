import { openSync, writeSync, closeSync } from "node:fs";

// Hook stdout belongs to the agent's protocol. Write to the PTY device Kumu
// injected; agent hooks may have no controlling terminal to fall back on.
export function notify(agent, message) {
  if (process.env.KUMU_TERMINAL !== "1") return;
  const path = process.env.KUMU_TTY || "/dev/tty";
  let fd;
  try {
    fd = openSync(path, "w");
    writeSync(fd, `\x1b]777;notify;${agent};${message}\x07`);
  } catch {
    // The PTY may already be closed, or the agent runs headless.
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
}
