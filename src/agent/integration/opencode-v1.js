import { notify } from "./notify.js";

export const KumuNotifications = async ({ client }) => {
  if (process.env.KUMU_TERMINAL !== "1") return {};
  const busy = new Set();
  return {
    event: async ({ event }) => {
      const data = event.properties;
      const id = data?.sessionID;
      if (!id) return;
      if (event.type === "session.status") {
        if (data.status.type === "busy") busy.add(id);
        return;
      }
      let message;
      if (event.type === "session.idle" && busy.delete(id)) message = "本轮任务已完成";
      if (event.type === "session.error" && busy.delete(id)) message = "任务执行失败，请查看终端";
      if (event.type === "permission.asked" || event.type === "permission.updated") message = "需要你确认操作权限";
      if (event.type === "question.asked") message = "需要你输入、确认或选择";
      if (!message) return;
      const response = await client.session.get({ path: { id } });
      if (response.data && !response.data.parentID) notify("OpenCode", message);
    },
  };
};
