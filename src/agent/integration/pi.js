import { notify, notifyStatus, notifyTitle } from "./notify.js";

// Pi and Oh My Pi share this module. OMP exposes ctx.agent; Pi does not, which
// also distinguishes them when either is started from a Kumu shell session.
const interactivePi = (ctx) => Boolean(ctx?.hasUI && !ctx.agent);
const interactiveOmp = (ctx) => ctx?.hasUI && ctx.agent?.kind === "main";

function hostName(ctx) {
  if (interactivePi(ctx)) return "Pi";
  if (interactiveOmp(ctx)) return "Oh My Pi";
  return null;
}

function reportSessionName(ctx) {
  const host = hostName(ctx);
  if (!host) return;
  try {
    const name = ctx?.sessionManager?.getSessionName?.();
    if (name) notifyTitle(host, name);
  } catch {
    // Ignore host session manager differences.
  }
}

function messageText(message) {
  if (!message) return "";
  if (typeof message.content === "string") return message.content;
  if (Array.isArray(message.content)) {
    const part = message.content.find((item) => item?.type === "text" && typeof item.text === "string");
    return part?.text || "";
  }
  return "";
}

export default function (pi) {
  if (process.env.KUMU_TERMINAL !== "1") return;

  let piBusy = false;
  let piStopReason;
  let ompBusy = false;
  let ompInterrupted = false;
  const wrapped = new WeakSet();

  const installDialogs = (ctx) => {
    if (!interactiveOmp(ctx) || !ctx.ui || typeof ctx.ui !== "object" || wrapped.has(ctx.ui)) {
      return;
    }
    wrapped.add(ctx.ui);
    for (const method of ["select", "confirm", "input", "editor", "ask", "custom"]) {
      const original = ctx.ui[method];
      if (typeof original !== "function") continue;
      try {
        ctx.ui[method] = function (...args) {
          const active = ompBusy;
          if (active) {
            notifyStatus("Oh My Pi", "waiting");
            notify("Oh My Pi", "需要你输入、确认或选择");
          }
          const result = original.apply(this, args);
          if (active) {
            Promise.resolve(result).finally(() => {
              if (ompBusy) notifyStatus("Oh My Pi", "resume");
            });
          }
          return result;
        };
      } catch {
        // Ignore frozen or non-writable UI objects so we never break the host.
      }
    }
  };

  pi.on("input", (event, ctx) => {
    const host = hostName(ctx);
    if (!host) return;
    const text = typeof event?.text === "string" ? event.text : typeof event?.prompt === "string" ? event.prompt : "";
    if (text) notifyTitle(host, text, { prompt: true });
  });

  pi.on("agent_start", (_event, ctx) => {
    if (interactivePi(ctx)) {
      piBusy = true;
      piStopReason = undefined;
      reportSessionName(ctx);
      notifyStatus("Pi", "running");
    }
    if (interactiveOmp(ctx)) {
      ompBusy = true;
      ompInterrupted = false;
      installDialogs(ctx);
      reportSessionName(ctx);
      notifyStatus("Oh My Pi", "running");
    }
  });
  pi.on("message_end", (event, ctx) => {
    const host = hostName(ctx);
    if (host && event?.message?.role === "user") {
      const text = messageText(event.message);
      if (text) notifyTitle(host, text, { prompt: true });
    }
    if (interactivePi(ctx) && event?.message?.role === "assistant") {
      piStopReason = event.message.stopReason;
    }
    if (interactiveOmp(ctx) && event?.message?.stopReason === "aborted") {
      ompInterrupted = true;
    }
  });
  // Pi emits these around ctx.ui.select/confirm/input/editor/custom, which
  // covers dialogs without monkey-patching the UI object.
  pi.on("ui_prompt_start", (_event, ctx) => {
    if (interactivePi(ctx)) {
      notifyStatus("Pi", "waiting");
      notify("Pi", "需要你输入、确认或选择");
    }
  });
  pi.on("ui_prompt_end", (_event, ctx) => {
    if (interactivePi(ctx) && piBusy) {
      notifyStatus("Pi", "resume");
    }
  });
  pi.on("tool_execution_start", (_event, ctx) => {
    if (interactivePi(ctx) && piBusy) notifyStatus("Pi", "resume");
    if (interactiveOmp(ctx) && ompBusy) notifyStatus("Oh My Pi", "resume");
  });
  // `agent_end` is not final: Pi may still retry, compact, or continue with
  // queued work. `agent_settled` is the documented final boundary.
  pi.on("agent_settled", (event, ctx) => {
    if (!interactivePi(ctx) || !piBusy) return;
    piBusy = false;
    if (event?.aborted) {
      notifyStatus("Pi", "idle");
      return;
    }
    if (ctx?.hasPendingMessages?.()) return;
    const failed = piStopReason === "error";
    notifyStatus("Pi", failed ? "failed" : "completed");
    notify("Pi", failed ? "任务执行失败，请查看终端" : "本轮任务已完成");
  });

  pi.on("session_start", (_event, ctx) => {
    installDialogs(ctx);
    const host = hostName(ctx);
    if (!host) return;
    reportSessionName(ctx);
    notifyStatus(host, "idle");
  });
  pi.on("session_shutdown", (_event, ctx) => {
    const host = hostName(ctx);
    if (!host) return;
    piBusy = false;
    ompBusy = false;
    notifyStatus(host, "exited");
  });
  pi.on("tool_approval_requested", (_event, ctx) => {
    if (interactiveOmp(ctx)) {
      notifyStatus("Oh My Pi", "waiting");
      notify("Oh My Pi", "需要你输入、确认或选择");
    }
  });
  pi.on("tool_call", (event, ctx) => {
    if (!interactiveOmp(ctx)) return;
    if (["ask", "ask_user", "ask_user_question", "question"].includes(event?.toolName)) {
      notifyStatus("Oh My Pi", "waiting");
      notify("Oh My Pi", "需要你输入、确认或选择");
    }
  });
  // OMP extends agent_end with `isTerminal`/`yielded`: intermediate ends while
  // it continues its own work must not claim completion.
  pi.on("agent_end", (event, ctx) => {
    if (!interactiveOmp(ctx) || !ompBusy) return;
    if (event?.yielded === false || (event?.isTerminal === false && event?.yielded !== true)) return;
    ompBusy = false;
    if (ompInterrupted) {
      notifyStatus("Oh My Pi", "idle");
      return;
    }
    if (ctx?.hasPendingMessages?.()) return;
    const last = event?.messages?.filter((message) => message?.role === "assistant").at(-1);
    const failed = last?.stopReason === "error";
    notifyStatus("Oh My Pi", failed ? "failed" : "completed");
    notify("Oh My Pi", failed ? "任务执行失败，请查看终端" : "本轮任务已完成");
  });
}
