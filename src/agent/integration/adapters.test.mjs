import { after, test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import piExtension from "./pi.js";
import openCode from "./opencode/tui.js";
import { KumuNotifications } from "./opencode-v1.js";

// Capture only the external PTY side effect; run the real event adapters.
const originals = { openSync: fs.openSync, writeSync: fs.writeSync, closeSync: fs.closeSync };
let messages = [];
fs.openSync = () => 42;
fs.writeSync = (_fd, message) => messages.push(message);
fs.closeSync = () => {};
syncBuiltinESMExports();
process.env.KUMU_TERMINAL = "1";
process.env.KUMU_TTY = "/dev/fake-kumu-tty";
after(() => {
  Object.assign(fs, originals);
  syncBuiltinESMExports();
});

test("Pi waits for agent_settled instead of claiming completion at agent_end", () => {
  messages = [];
  const handlers = {};
  piExtension({ on: (name, callback) => { handlers[name] = callback; } });
  const ctx = { hasUI: true, hasPendingMessages: () => false, ui: {} };
  handlers.agent_start({}, ctx);
  handlers.agent_end({ messages: [] }, ctx);
  assert.equal(messages.length, 0);
  handlers.agent_settled({ aborted: false }, ctx);
  assert.match(messages.at(-1), /本轮任务已完成/);
  handlers.agent_settled({ aborted: false }, ctx);
  assert.equal(messages.length, 1, "one completion per prompt");
});

test("Pi reports errors, cancellation, queued input and headless work correctly", () => {
  messages = [];
  const handlers = {};
  piExtension({ on: (name, callback) => { handlers[name] = callback; } });
  const ctx = { hasUI: true, hasPendingMessages: () => false, ui: {} };
  handlers.agent_start({}, ctx);
  handlers.message_end({ message: { role: "assistant", stopReason: "error" } }, ctx);
  handlers.agent_settled({ aborted: false }, ctx);
  assert.match(messages.at(-1), /任务执行失败/);
  handlers.agent_start({}, ctx);
  handlers.agent_settled({ aborted: true }, ctx);
  handlers.agent_start({}, ctx);
  handlers.agent_settled({ aborted: false }, { ...ctx, hasPendingMessages: () => true });
  handlers.agent_start({}, { ...ctx, hasUI: false });
  handlers.agent_settled({ aborted: false }, { ...ctx, hasUI: false });
  handlers.agent_start({}, { ...ctx, agent: { kind: "sub" } });
  handlers.agent_settled({ aborted: false }, { ...ctx, agent: { kind: "sub" } });
  assert.equal(messages.length, 1);
});

test("Pi uses ui_prompt_start instead of tearing the UI object", async () => {
  messages = [];
  const handlers = {};
  piExtension({ on: (name, callback) => { handlers[name] = callback; } });
  const ui = { confirm: async () => true };
  const ctx = { hasUI: true, hasPendingMessages: () => false, ui };
  handlers.agent_start({}, ctx);
  handlers.ui_prompt_start({ kind: "confirm" }, ctx);
  assert.match(messages.at(-1), /需要你输入、确认或选择/);
  assert.equal(await ui.confirm(), true, "the dialog still runs");
});

test("OMP keeps its intermediate agent_end filtering and dialog bridge", async () => {
  messages = [];
  const handlers = {};
  piExtension({ on: (name, callback) => { handlers[name] = callback; } });
  let dialogs = 0;
  const ctx = {
    hasUI: true,
    agent: { kind: "main" },
    hasPendingMessages: () => false,
    ui: { confirm: async () => { dialogs++; return false; } },
  };
  handlers.session_start({}, ctx);
  handlers.agent_start({}, ctx);
  handlers.tool_approval_requested({}, ctx);
  assert.match(messages.at(-1), /需要你输入、确认或选择/);
  messages = [];
  assert.equal(await ctx.ui.confirm("Approval", "Proceed?"), false);
  assert.equal(dialogs, 1);
  assert.equal(messages.length, 1);
  handlers.agent_end({ messages: [], isTerminal: false, yielded: false }, ctx);
  assert.equal(messages.length, 1, "continuations are not complete");
  handlers.agent_end({ messages: [] }, ctx);
  assert.match(messages.at(-1), /本轮任务已完成/);
  handlers.agent_end({ messages: [] }, ctx);
  assert.equal(messages.length, 2, "one completion per prompt");
  handlers.agent_start({}, { ...ctx, agent: { kind: "sub" } });
  handlers.agent_end({ messages: [] }, { ...ctx, agent: { kind: "sub" } });
  assert.equal(messages.length, 2, "subagents are silent");
});

test("the shared extension tells Pi and OMP apart from ctx.agent", () => {
  messages = [];
  const handlers = {};
  piExtension({ on: (name, callback) => { handlers[name] = callback; } });
  const piCtx = { hasUI: true, hasPendingMessages: () => false, ui: {} };
  handlers.agent_start({}, piCtx);
  handlers.agent_settled({ aborted: false }, piCtx);
  assert.match(messages.at(-1), /\x1b\]777;notify;Pi;/);
  const ompCtx = { ...piCtx, agent: { kind: "main" } };
  handlers.agent_start({}, ompCtx);
  handlers.agent_end({ messages: [] }, ompCtx);
  assert.match(messages.at(-1), /\x1b\]777;notify;Oh My Pi;/);
});

test("OpenCode 2 scopes events to root sessions and cleans up subscriptions", () => {
  messages = [];
  const handlers = {};
  let disposed = 0;
  const cleanup = openCode.setup({ data: {
    on: (name, callback) => { handlers[name] = callback; return () => disposed++; },
    session: { get: (id) => id === "root" ? {} : { parentID: "root" } },
  } });
  handlers["permission.asked"]({ data: { sessionID: "root" } });
  handlers["form.created"]({ data: { form: { sessionID: "root" } } });
  handlers["session.execution.started"]({ data: { sessionID: "root" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "root" } });
  handlers["session.execution.started"]({ data: { sessionID: "sub" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "sub" } });
  assert.equal(messages.length, 3);
  assert.match(messages[0], /确认操作权限/);
  assert.match(messages[1], /输入、确认或选择/);
  assert.match(messages[2], /任务已完成/);
  cleanup();
  assert.equal(disposed, 6);
});

test("OpenCode 1 distinguishes completion, questions, permissions and failures", async () => {
  messages = [];
  const adapter = await KumuNotifications({ client: { session: {
    get: async () => ({ data: {} }),
  } } });
  const emit = (type, properties = {}) => adapter.event({ event: { type, properties: { sessionID: "root", ...properties } } });
  await emit("session.idle");
  assert.equal(messages.length, 0, "startup idle is not a completed task");
  await emit("session.status", { status: { type: "busy" } });
  await emit("session.idle");
  await emit("permission.asked");
  await emit("question.asked");
  await emit("session.status", { status: { type: "busy" } });
  await emit("session.error");
  await emit("session.idle");
  assert.equal(messages.length, 4);
  assert.match(messages[0], /已完成/);
  assert.match(messages[3], /失败/);
});

test("globally installed integrations leave other terminals untouched", async () => {
  delete process.env.KUMU_TERMINAL;
  try {
    piExtension({ on: () => assert.fail("registered outside Kumu") });
    openCode.setup({ data: { on: () => assert.fail("registered outside Kumu") } });
    assert.deepEqual(await KumuNotifications({}), {});
  } finally {
    process.env.KUMU_TERMINAL = "1";
  }
});
