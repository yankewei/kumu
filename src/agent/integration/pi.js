import { notify } from "./notify.js";

// Pi and Oh My Pi share this module. OMP exposes ctx.agent; Pi does not, which
// also distinguishes them when either is started from a Kumu shell session.
const interactivePi = (ctx) => Boolean(ctx?.hasUI && !ctx.agent);
const interactiveOmp = (ctx) => ctx?.hasUI && ctx.agent?.kind === "main";

export default function (pi) {
  if (process.env.KUMU_TERMINAL !== "1") return;

  let piBusy = false;
  let piStopReason;
  let ompBusy = false;
  let ompInterrupted = false;
  const wrapped = new WeakSet();

  const installDialogs = (ctx) => {
    if (!interactiveOmp(ctx) || wrapped.has(ctx.ui)) return;
    wrapped.add(ctx.ui);
    for (const method of ["select", "confirm", "input", "editor", "ask", "custom"]) {
      const original = ctx.ui[method];
      if (typeof original !== "function") continue;
      ctx.ui[method] = function (...args) {
        if (ompBusy) notify("Oh My Pi", "需要你输入、确认或选择");
        return original.apply(this, args);
      };
    }
  };

  pi.on("agent_start", (_event, ctx) => {
    if (interactivePi(ctx)) {
      piBusy = true;
      piStopReason = undefined;
    }
    if (interactiveOmp(ctx)) {
      ompBusy = true;
      ompInterrupted = false;
      installDialogs(ctx);
    }
  });
  pi.on("message_end", (event, ctx) => {
    if (interactivePi(ctx) && event.message?.role === "assistant") {
      piStopReason = event.message.stopReason;
    }
    if (interactiveOmp(ctx) && event.message?.stopReason === "aborted") {
      ompInterrupted = true;
    }
  });
  // Pi emits these around ctx.ui.select/confirm/input/editor/custom, which
  // covers dialogs without monkey-patching the UI object.
  pi.on("ui_prompt_start", (_event, ctx) => {
    if (interactivePi(ctx)) notify("Pi", "需要你输入、确认或选择");
  });
  // `agent_end` is not final: Pi may still retry, compact, or continue with
  // queued work. `agent_settled` is the documented final boundary.
  pi.on("agent_settled", (event, ctx) => {
    if (!interactivePi(ctx) || !piBusy) return;
    piBusy = false;
    if (event.aborted || ctx.hasPendingMessages()) return;
    notify("Pi", piStopReason === "error" ? "任务执行失败，请查看终端" : "本轮任务已完成");
  });

  pi.on("session_start", (_event, ctx) => installDialogs(ctx));
  pi.on("tool_approval_requested", (_event, ctx) => {
    if (interactiveOmp(ctx)) notify("Oh My Pi", "需要你输入、确认或选择");
  });
  pi.on("tool_call", (event, ctx) => {
    if (!interactiveOmp(ctx)) return;
    if (["ask", "ask_user", "ask_user_question", "question"].includes(event.toolName)) {
      notify("Oh My Pi", "需要你输入、确认或选择");
    }
  });
  // OMP extends agent_end with `isTerminal`/`yielded`: intermediate ends while
  // it continues its own work must not claim completion.
  pi.on("agent_end", (event, ctx) => {
    if (!interactiveOmp(ctx) || !ompBusy) return;
    if (event.yielded === false || (event.isTerminal === false && event.yielded !== true)) return;
    ompBusy = false;
    if (ompInterrupted || ctx.hasPendingMessages()) return;
    const last = event.messages?.filter((message) => message.role === "assistant").at(-1);
    notify("Oh My Pi", last?.stopReason === "error" ? "任务执行失败，请查看终端" : "本轮任务已完成");
  });
}
