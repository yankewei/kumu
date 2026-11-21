import { notify, notifySession, notifyStatus, notifyTitle } from "./notify.js";

const MAX_TRACKED = 512;

function remember(set, id) {
  if (set.size >= MAX_TRACKED) {
    const oldest = set.values().next().value;
    if (oldest !== undefined) set.delete(oldest);
  }
  set.add(id);
}

export const KumuNotifications = async ({ client }) => {
  if (process.env.KUMU_TERMINAL !== "1") return {};
  const busy = new Set();
  const rootSession = async (id) => {
    const response = await client?.session?.get?.({ path: { id } });
    return response?.data && !response.data.parentID ? response.data : null;
  };
  return {
    event: async ({ event } = {}) => {
      try {
        const data = event?.properties;
        const id = data?.sessionID || data?.info?.id;
        if (!id) return;
        if (event.type === "session.updated") {
          const info = data?.info;
          if (info && !info.parentID && info.title) {
            notifyTitle("OpenCode", info.title);
          }
          return;
        }
        if (event.type === "session.status") {
          if (data?.status?.type === "busy") {
            const session = await rootSession(id);
            if (!session) return;
            remember(busy, id);
            notifySession("OpenCode", id);
            if (session.title) notifyTitle("OpenCode", session.title);
            notifyStatus("OpenCode", "running");
          } else {
            busy.delete(id);
          }
          return;
        }
        if (event.type === "session.deleted") {
          busy.delete(id);
          return;
        }
        if (event.type === "permission.replied" || event.type === "question.replied") {
          if (busy.has(id)) notifyStatus("OpenCode", "resume");
          return;
        }
        let message;
        let state;
        if (event.type === "session.idle" && busy.delete(id)) {
          message = "本轮任务已完成";
          state = "completed";
        }
        if (event.type === "session.error" && busy.delete(id)) {
          message = "任务执行失败，请查看终端";
          state = "failed";
        }
        if (event.type === "permission.asked" || event.type === "permission.updated") {
          message = "需要你确认操作权限";
          state = "waiting";
        }
        if (event.type === "question.asked") {
          message = "需要你输入、确认或选择";
          state = "waiting";
        }
        if (!message) return;
        const session = await rootSession(id);
        if (session) {
          notifySession("OpenCode", id);
          if (session.title) notifyTitle("OpenCode", session.title);
          notifyStatus("OpenCode", state);
          notify("OpenCode", message);
        }
      } catch {
        // Never leak unhandled rejections into the OpenCode runtime.
      }
    },
  };
};
