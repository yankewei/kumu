import { realpathSync } from "node:fs";
import { resolve } from "node:path";
import { notify, notifySession } from "../notify.js";

// The same directory reaches us with and without resolved symlinks.
const canonical = (path) => {
  try {
    return realpathSync(path);
  } catch {
    return resolve(path);
  }
};

// OpenCode 2's TUI owns the PTY even when its server is shared or remote.
export default {
  id: "kumu.notifications",
  setup(ctx) {
    if (process.env.KUMU_TERMINAL !== "1") return;
    const ended = new Set();
    // Every attached TUI receives every location's events. Notify only for
    // this TUI's own project, otherwise each open pane raises its own copy.
    // `ctx.location` is optional in OpenCode's TUI context, and
    // `data.location.default()` is the server's — one location shared by
    // every attached TUI — so it can never say which pane this is. The pane
    // started this process in its project directory: that is `process.cwd()`.
    // Read the location per event; it can arrive after setup.
    const here = () => ctx.location?.directory || process.cwd();
    const relevant = (id) => {
      const session = ctx.data.session.get(id);
      if (!session || session.parentID) return false;
      const hereProject = ctx.location?.project?.id;
      if (hereProject && session.projectID) return hereProject === session.projectID;
      const there = session.directory || session.location?.directory;
      // Without the session's directory nothing proves this pane owns it;
      // silence beats echoing another pane's conversation.
      if (!there) return false;
      return canonical(here()) === canonical(there);
    };
    const end = (event, message) => {
      const id = event.data.sessionID;
      if (ended.has(id) || !relevant(id)) return;
      ended.add(id);
      notify("OpenCode", message);
    };
    const dispose = [
      ctx.data.on("session.execution.started", (event) => {
        const id = event.data.sessionID;
        if (relevant(id)) notifySession("OpenCode", id);
        ended.delete(id);
      }),
      ctx.data.on("session.execution.succeeded", (event) => end(event, "本轮任务已完成")),
      ctx.data.on("session.execution.failed", (event) => end(event, "任务执行失败，请查看终端")),
      ctx.data.on("session.execution.interrupted", (event) => ended.add(event.data.sessionID)),
      ctx.data.on("permission.asked", (event) => {
        if (relevant(event.data.sessionID)) notify("OpenCode", "需要你确认操作权限");
      }),
      ctx.data.on("form.created", (event) => {
        if (relevant(event.data.form.sessionID)) notify("OpenCode", "需要你输入、确认或选择");
      }),
    ];
    return () => dispose.reverse().forEach((dispose) => dispose());
  },
};
