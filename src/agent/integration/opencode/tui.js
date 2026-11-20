import { realpathSync } from "node:fs";
import { resolve } from "node:path";
import { notify, notifySession } from "../notify.js";

const MAX_TRACKED = 512;
const canonicalCache = new Map();

// The same directory reaches us with and without resolved symlinks.
const canonical = (path) => {
  if (!path) return "";
  const hit = canonicalCache.get(path);
  if (hit !== undefined) return hit;
  let result;
  try {
    result = realpathSync(path);
  } catch {
    result = resolve(path);
  }
  if (canonicalCache.size >= MAX_TRACKED) {
    const oldest = canonicalCache.keys().next().value;
    if (oldest !== undefined) canonicalCache.delete(oldest);
  }
  canonicalCache.set(path, result);
  return result;
};

function remember(set, id) {
  if (!id) return;
  if (set.size >= MAX_TRACKED) {
    const oldest = set.values().next().value;
    if (oldest !== undefined) set.delete(oldest);
  }
  set.add(id);
}

function safeCwd() {
  try {
    return process.cwd();
  } catch {
    return "";
  }
}

// OpenCode 2's TUI owns the PTY even when its server is shared or remote.
export default {
  id: "kumu.notifications",
  setup(ctx) {
    if (process.env.KUMU_TERMINAL !== "1") return;
    const ended = new Set();
    const here = () => ctx?.location?.directory || safeCwd();
    const relevant = (id) => {
      if (!id) return false;
      const session = ctx?.data?.session?.get?.(id);
      if (!session || session.parentID) return false;
      const hereProject = ctx?.location?.project?.id;
      const there = session.directory || session.location?.directory;
      if (hereProject && session.projectID) {
        if (hereProject !== session.projectID) return false;
        // Two git worktrees can share the same project ID; when directories
        // are available, require them to match too.
        const currentDir = here();
        if (currentDir && there) {
          return canonical(currentDir) === canonical(there);
        }
        return true;
      }
      // Without the session's directory nothing proves this pane owns it;
      // silence beats echoing another pane's conversation.
      const currentDir = here();
      if (!there || !currentDir) return false;
      return canonical(currentDir) === canonical(there);
    };
    const end = (event, message) => {
      const id = event?.data?.sessionID;
      if (!id || ended.has(id) || !relevant(id)) return;
      remember(ended, id);
      notify("OpenCode", message);
    };
    const dispose = [
      ctx.data.on("session.execution.started", (event) => {
        const id = event?.data?.sessionID;
        if (!id) return;
        if (relevant(id)) notifySession("OpenCode", id);
        ended.delete(id);
      }),
      ctx.data.on("session.execution.succeeded", (event) => end(event, "本轮任务已完成")),
      ctx.data.on("session.execution.failed", (event) => end(event, "任务执行失败，请查看终端")),
      ctx.data.on("session.execution.interrupted", (event) => {
        const id = event?.data?.sessionID;
        if (id && relevant(id)) remember(ended, id);
      }),
      ctx.data.on("permission.asked", (event) => {
        if (relevant(event?.data?.sessionID)) notify("OpenCode", "需要你确认操作权限");
      }),
      ctx.data.on("form.created", (event) => {
        if (relevant(event?.data?.form?.sessionID)) notify("OpenCode", "需要你输入、确认或选择");
      }),
    ];
    return () => dispose.reverse().forEach((dispose) => dispose());
  },
};
