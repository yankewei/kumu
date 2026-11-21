import { after, test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import tty from "node:tty";
import { syncBuiltinESMExports } from "node:module";
import { notify, notifySession, notifyTitle } from "./notify.js";
import piExtension from "./pi.js";
import openCode from "./opencode/tui.js";
import { KumuNotifications } from "./opencode-v1.js";

// Capture only the external PTY side effect; run the real event adapters.
const originals = {
  openSync: fs.openSync,
  writeSync: fs.writeSync,
  closeSync: fs.closeSync,
  isatty: tty.isatty,
};
let messages = [];
fs.openSync = () => 42;
fs.writeSync = (_fd, message) => messages.push(message);
fs.closeSync = () => {};
tty.isatty = () => true;
syncBuiltinESMExports();
process.env.KUMU_TERMINAL = "1";
delete process.env.KUMU_SESSION_TOKEN;
// Session and status updates travel on the same channel but are not shown to
// the user.
const statuses = () => messages.filter((message) => message.includes(";kumu-status:"));
const titles = () => messages.filter((message) => message.includes(";kumu-title:"));
const visible = () =>
  messages.filter(
    (message) =>
      !message.includes(";kumu-session:") &&
      !message.includes(";kumu-status:") &&
      !message.includes(";kumu-title:"),
  );
process.env.KUMU_TTY = "/dev/fake-kumu-tty";
after(() => {
  Object.assign(fs, {
    openSync: originals.openSync,
    writeSync: originals.writeSync,
    closeSync: originals.closeSync,
  });
  tty.isatty = originals.isatty;
  syncBuiltinESMExports();
});

test("Pi waits for agent_settled instead of claiming completion at agent_end", () => {
  messages = [];
  const handlers = {};
  piExtension({ on: (name, callback) => { handlers[name] = callback; } });
  const ctx = { hasUI: true, hasPendingMessages: () => false, ui: {} };
  handlers.agent_start({}, ctx);
  handlers.agent_end({ messages: [] }, ctx);
  assert.equal(visible().length, 0);
  assert.match(statuses().at(-1), /kumu-status:running/);
  handlers.agent_settled({ aborted: false }, ctx);
  assert.match(messages.at(-1), /本轮任务已完成/);
  assert.match(statuses().at(-1), /kumu-status:completed/);
  handlers.agent_settled({ aborted: false }, ctx);
  assert.equal(visible().length, 1, "one completion per prompt");
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
  assert.equal(visible().length, 1);
  assert.ok(
    statuses().some((message) => message.includes("kumu-status:failed")),
    "an error turn reports failed",
  );
  assert.ok(
    statuses().some((message) => message.includes("kumu-status:idle")),
    "an aborted turn returns to idle",
  );
});

test("Pi uses ui_prompt_start/end and reports prompt titles", async () => {
  messages = [];
  const handlers = {};
  piExtension({ on: (name, callback) => { handlers[name] = callback; } });
  const ui = { confirm: async () => true };
  const ctx = { hasUI: true, hasPendingMessages: () => false, ui };
  handlers.input({ text: "Refactor agent list" }, ctx);
  handlers.agent_start({}, ctx);
  handlers.ui_prompt_start({ kind: "confirm" }, ctx);
  assert.match(messages.at(-1), /需要你输入、确认或选择/);
  assert.match(statuses().at(-1), /kumu-status:waiting/);
  assert.equal(await ui.confirm(), true, "the dialog still runs");
  handlers.ui_prompt_end({}, ctx);
  assert.match(statuses().at(-1), /kumu-status:resume/);
  assert.match(titles()[0], /kumu-title:prompt:Refactor agent list\x07$/);
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
  await Promise.resolve();
  assert.equal(dialogs, 1);
  assert.equal(visible().length, 1);
  assert.match(statuses().at(-1), /kumu-status:resume/);
  handlers.agent_end({ messages: [], isTerminal: false, yielded: false }, ctx);
  assert.equal(visible().length, 1, "continuations are not complete");
  handlers.agent_end({ messages: [] }, ctx);
  assert.match(messages.at(-1), /本轮任务已完成/);
  handlers.agent_end({ messages: [] }, ctx);
  assert.equal(visible().length, 2, "one completion per prompt");
  handlers.agent_start({}, { ...ctx, agent: { kind: "sub" } });
  handlers.agent_end({ messages: [] }, { ...ctx, agent: { kind: "sub" } });
  assert.equal(visible().length, 2, "subagents are silent");
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
    session: { get: (id) => id === "root" ? { directory: process.cwd(), title: "Fix sidebar" } : { parentID: "root" } },
  } });
  handlers["permission.asked"]({ data: { sessionID: "root" } });
  handlers["permission.replied"]({ data: { sessionID: "root" } });
  handlers["form.created"]({ data: { form: { sessionID: "root" } } });
  handlers["session.execution.started"]({ data: { sessionID: "root" } });
  handlers["session.updated"]({ data: { info: { id: "root", title: "Global agent list" } } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "root" } });
  handlers["session.execution.started"]({ data: { sessionID: "root" } });
  handlers["session.execution.interrupted"]({ data: { sessionID: "root" } });
  handlers["session.execution.started"]({ data: { sessionID: "sub" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "sub" } });
  assert.equal(visible().length, 3);
  assert.match(visible()[0], /确认操作权限/);
  assert.match(visible()[1], /输入、确认或选择/);
  assert.match(visible()[2], /任务已完成/);
  const sessionMessages = messages.filter((message) => message.includes("kumu-session:"));
  assert.equal(sessionMessages.length, 2);
  assert.match(sessionMessages[0], /kumu-session:root\x07$/);
  assert.ok(
    sessionMessages.every((message) => !message.includes("kumu-session:sub")),
    "a sub-session is never reported as the pane's conversation",
  );
  assert.ok(titles().some((message) => message.includes("kumu-title:Global agent list")));
  assert.ok(statuses().some((message) => message.includes("kumu-status:resume")));
  assert.ok(statuses().some((message) => message.includes("kumu-status:idle")));
  cleanup();
  assert.equal(disposed, 9);
});

test("OpenCode 2 ignores sessions in another TUI's project", () => {
  messages = [];
  const handlers = {};
  const cleanup = openCode.setup({
    location: { directory: "/work/here", project: { id: "mine" } },
    data: {
      on: (name, callback) => { handlers[name] = callback; return () => {}; },
      session: { get: (id) => ({
        projectID: id === "mine" ? "mine" : "elsewhere",
        location: { directory: id === "mine" ? "/work/here" : "/work/elsewhere" },
      }) },
    },
  });
  handlers["session.execution.started"]({ data: { sessionID: "mine" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "mine" } });
  handlers["session.execution.started"]({ data: { sessionID: "elsewhere" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "elsewhere" } });
  handlers["permission.asked"]({ data: { sessionID: "elsewhere" } });
  handlers["form.created"]({ data: { form: { sessionID: "elsewhere" } } });
  assert.equal(visible().length, 1, "only this TUI's project notifies");
  assert.match(visible()[0], /任务已完成/);
  assert.equal(
    messages.filter((message) => message.includes("kumu-session:")).length,
    1,
    "only this TUI's project reports a conversation",
  );
  cleanup();
});

test("OpenCode 2 matches the directory when project IDs are unavailable", () => {
  messages = [];
  const handlers = {};
  const cleanup = openCode.setup({
    location: { directory: "/work/here" },
    data: {
      on: (name, callback) => { handlers[name] = callback; return () => {}; },
      session: { get: (id) => ({ location: { directory: id === "here" ? "/work/here" : "/work/elsewhere" } }) },
    },
  });
  handlers["session.execution.started"]({ data: { sessionID: "elsewhere" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "elsewhere" } });
  assert.equal(messages.length, 0);
  handlers["session.execution.started"]({ data: { sessionID: "here" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "here" } });
  assert.equal(visible().length, 1);
  cleanup();
});

test("OpenCode 2 reads the pane's own directory when the TUI location is missing", () => {
  messages = [];
  const handlers = {};
  const location = {};
  const here = process.cwd();
  const cleanup = openCode.setup({
    get location() { return location; },
    data: {
      on: (name, callback) => { handlers[name] = callback; return () => {}; },
      // The server's default location is shared by every attached TUI, so it
      // must not decide which pane an event belongs to.
      location: { default: () => ({ directory: here }) },
      session: { get: (id) => ({ directory: id }) },
    },
  });
  handlers["session.execution.started"]({ data: { sessionID: "/work/elsewhere" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "/work/elsewhere" } });
  assert.equal(messages.length, 0, "another project's session stays silent");
  handlers["session.execution.started"]({ data: { sessionID: here } });
  handlers["session.execution.succeeded"]({ data: { sessionID: here } });
  assert.equal(visible().length, 1, "the pane's own session notifies before location loads");
  location.directory = "/work/next";
  handlers["session.execution.started"]({ data: { sessionID: here } });
  handlers["session.execution.succeeded"]({ data: { sessionID: here } });
  handlers["session.execution.started"]({ data: { sessionID: "/work/next" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "/work/next" } });
  assert.equal(visible().length, 2, "a later directory change is not ignored");
  cleanup();
});

test("OpenCode 2 stays silent for a session it cannot place in a directory", () => {
  messages = [];
  const handlers = {};
  const cleanup = openCode.setup({ data: {
    on: (name, callback) => { handlers[name] = callback; return () => {}; },
    location: { default: () => ({ directory: process.cwd() }) },
    session: { get: () => ({}) },
  } });
  handlers["session.execution.started"]({ data: { sessionID: "nowhere" } });
  handlers["session.execution.succeeded"]({ data: { sessionID: "nowhere" } });
  handlers["permission.asked"]({ data: { sessionID: "nowhere" } });
  handlers["form.created"]({ data: { form: { sessionID: "nowhere" } } });
  assert.equal(messages.length, 0, "no directory means no proof this pane owns the session");
  cleanup();
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
  assert.equal(visible().length, 4);
  assert.match(visible()[0], /已完成/);
  assert.match(visible()[3], /失败/);
  assert.equal(
    messages.filter((message) => message.includes("kumu-session:root")).length,
    6,
    "root starts, completions, prompts and failures all report the conversation",
  );
});

test("globally installed integrations leave other terminals untouched", async () => {
  delete process.env.KUMU_TERMINAL;
  try {
    piExtension({ on: () => assert.fail("registered outside Kumu") });
    openCode.setup({ data: { on: () => assert.fail("registered outside Kumu") } });
    assert.deepEqual(await KumuNotifications({}), {});
  } finally {
    process.env.KUMU_TERMINAL = "1";
delete process.env.KUMU_SESSION_TOKEN;
  }
});

test("notify sanitizes control chars and notifySession rejects option injection", () => {
  messages = [];
  notify("Open;Code\x07", "done\x1b]52;c;evil\x07");
  assert.equal(messages.length, 1);
  assert.equal(messages[0], "\x1b]777;notify;OpenCode;done]52cevil\x07");

  messages = [];
  notifySession("OpenCode", "--dangerously-skip-permissions");
  notifySession("OpenCode", "../escape");
  assert.equal(messages.length, 0);

  process.env.KUMU_SESSION_TOKEN = "tok_abc";
  try {
    notifySession("OpenCode", "ses-1");
    assert.equal(messages[0], "\x1b]777;notify;OpenCode;kumu-session:tok_abc:ses-1\x07");
  } finally {
    delete process.env.KUMU_SESSION_TOKEN;
  }

  messages = [];
  notifyTitle("Pi", "/clear", { prompt: true });
  notifyTitle("Pi", "  First line;clean\nsecond line", { prompt: true });
  notifyTitle("OpenCode", "Session summary");
  assert.deepEqual(messages, [
    "\x1b]777;notify;Pi;kumu-title:prompt:First lineclean\x07",
    "\x1b]777;notify;OpenCode;kumu-title:Session summary\x07",
  ]);
});
