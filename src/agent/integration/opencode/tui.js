import { realpathSync } from "node:fs";
import { resolve } from "node:path";
import { notify, notifySession, notifyStatus, notifyTitle } from "../notify.js";

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
    const relevantSession = (id) => {
      if (!id) return null;
      const session = ctx?.data?.session?.get?.(id);
      if (!session || session.parentID) return null;
      const hereProject = ctx?.location?.project?.id;
      const there = session.directory || session.location?.directory;
      if (hereProject && session.projectID) {
        if (hereProject !== session.projectID) return null;
        // Two git worktrees can share the same project ID; when directories
        // are available, require them to match too.
        const currentDir = here();
        if (currentDir && there && canonical(currentDir) !== canonical(there)) {
          return null;
        }
        return session;
      }
      // Without the session's directory nothing proves this pane owns it;
      // silence beats echoing another pane's conversation.
      const currentDir = here();
      if (!there || !currentDir) return null;
      return canonical(currentDir) === canonical(there) ? session : null;
    };
    const reportTitle = (session) => {
      if (session?.title) notifyTitle("OpenCode", session.title);
    };
    const end = (event, message, state) => {
      const id = event?.data?.sessionID;
      if (!id || ended.has(id)) return;
      const session = relevantSession(id);
      if (!session) return;
      remember(ended, id);
      reportTitle(session);
      notifyStatus("OpenCode", state);
      notify("OpenCode", message);
    };
    const resumeIfActive = (id) => {
      if (!id || ended.has(id) || !relevantSession(id)) return;
      notifyStatus("OpenCode", "resume");
    };
    const dispose = [
      ctx.data.on("session.execution.started", (event) => {
        const id = event?.data?.sessionID;
        if (!id) return;
        const session = relevantSession(id);
        if (session) {
          notifySession("OpenCode", id);
          reportTitle(session);
          notifyStatus("OpenCode", "running");
        }
        ended.delete(id);
      }),
      ctx.data.on("session.updated", (event) => {
        const id = event?.data?.sessionID || event?.data?.info?.id || event?.data?.id;
        const session = relevantSession(id);
        if (!session) return;
        reportTitle(event?.data?.info?.title ? event.data.info : session);
      }),
      ctx.data.on("session.execution.succeeded", (event) =>
        end(event, "本轮任务已完成", "completed"),
      ),
      ctx.data.on("session.execution.failed", (event) =>
        end(event, "任务执行失败，请查看终端", "failed"),
      ),
      ctx.data.on("session.execution.interrupted", (event) => {
        const id = event?.data?.sessionID;
        if (!id || !relevantSession(id)) return;
        remember(ended, id);
        notifyStatus("OpenCode", "idle");
      }),
      ctx.data.on("permission.asked", (event) => {
        const session = relevantSession(event?.data?.sessionID);
        if (!session) return;
        reportTitle(session);
        notifyStatus("OpenCode", "waiting");
        notify("OpenCode", "需要你确认操作权限");
      }),
      ctx.data.on("permission.replied", (event) =>
        resumeIfActive(event?.data?.sessionID),
      ),
      ctx.data.on("form.created", (event) => {
        const session = relevantSession(event?.data?.form?.sessionID);
        if (!session) return;
        reportTitle(session);
        notifyStatus("OpenCode", "waiting");
        notify("OpenCode", "需要你输入、确认或选择");
      }),
      ctx.data.on("form.submitted", (event) =>
        resumeIfActive(event?.data?.form?.sessionID || event?.data?.sessionID),
      ),
    ];
    return () => dispose.reverse().forEach((dispose) => dispose());
  },
};
