import { notify } from "../notify.js";

// OpenCode 2's TUI owns the PTY even when its server is shared or remote.
export default {
  id: "kumu.notifications",
  setup(ctx) {
    if (process.env.KUMU_TERMINAL !== "1") return;
    const ended = new Set();
    const relevant = (id) => {
      const session = ctx.data.session.get(id);
      return session && !session.parentID;
    };
    const end = (event, message) => {
      const id = event.data.sessionID;
      if (ended.has(id)) return;
      ended.add(id);
      if (relevant(id)) notify("OpenCode", message);
    };
    const dispose = [
      ctx.data.on("session.execution.started", (event) => ended.delete(event.data.sessionID)),
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
