#!/usr/bin/env python3
"""Verify hook transport without prescribing an Agent launch command.

Run after `cargo build`: python3 scripts/test-agent-integration.py

Both Claude and Codex run command hooks in a new session without a
controlling terminal. Claude therefore receives `terminalSequence` on stdout;
Codex, Grok Build, Cursor and Antigravity CLI write to the explicit PTY path
in `KUMU_TTY`.
"""
import json
import os
from pathlib import Path
import pty
import select
import subprocess
import tempfile
import unittest

BINARY = Path(__file__).resolve().parents[1] / "target/debug/kumu"


def detached_env(**extra):
    env = dict(os.environ, KUMU_TERMINAL="1", KUMU_APP_BINARY=str(BINARY))
    env.pop("KUMU_TTY", None)
    # Running these tests inside a Kumu pane must not add the pane's token to
    # the expected sequences.
    env.pop("KUMU_SESSION_TOKEN", None)
    env.update(extra)
    return env


def run_hook(args, env):
    # setsid() reproduces the agents' hook process: no controlling terminal.
    return subprocess.run(
        [BINARY, "--kumu-notify", *args],
        env=env,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        timeout=5,
        preexec_fn=os.setsid,
    )


def read_pty(master):
    ready, _, _ = select.select([master], [], [], 5)
    if not ready:
        return ""
    return os.read(master, 65536).decode()


class AgentIntegrationTests(unittest.TestCase):
    def test_claude_returns_terminal_sequence_on_stdout(self):
        for event, message in [
            ("complete", "本轮任务已结束"),
            ("permission", "确认操作权限"),
            ("input", "输入、确认或选择"),
        ]:
            with self.subTest(event=event):
                result = run_hook(["claude", event], detached_env())
                self.assertEqual((result.returncode, result.stderr), (0, b""))
                payload = json.loads(result.stdout)
                sequence = payload["terminalSequence"]
                self.assertTrue(sequence.startswith(f"\x1b]777;notify;claude;"))
                self.assertIn(message, sequence)
                self.assertTrue(sequence.endswith("\x07"))

    def test_hook_agents_write_to_the_injected_tty_without_a_controlling_terminal(self):
        for agent in ("codex", "grok", "cursor", "agy"):
            for event, message in [
                ("complete", "本轮任务已结束"),
                ("permission", "确认操作权限"),
                ("input", "输入、确认或选择"),
            ]:
                with self.subTest(agent=agent, event=event):
                    master, slave = pty.openpty()
                    try:
                        env = detached_env(KUMU_TTY=os.ttyname(slave))
                        process = subprocess.Popen(
                            [BINARY, "--kumu-notify", agent, event],
                            env=env,
                            stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            preexec_fn=os.setsid,
                        )
                        output = read_pty(master)
                        stdout, stderr = process.communicate(timeout=5)
                    finally:
                        os.close(slave)
                        os.close(master)
                    self.assertEqual((process.returncode, stdout, stderr), (0, b"", b""))
                    self.assertIn(f"\x1b]777;notify;{agent};", output)
                    self.assertIn(message, output)
                    self.assertTrue(output.endswith("\x07"))

    def test_hooks_from_absolute_custom_scripts_preserve_the_users_arguments(self):
        with tempfile.TemporaryDirectory(prefix="kumu hooks '") as root:
            script = Path(root) / "my-agent-launcher"
            script.write_text('''#!/bin/sh
printf 'ORIGINAL=<%s>\\n' "$@"
"$KUMU_APP_BINARY" --kumu-notify codex permission
''')
            script.chmod(0o755)
            master, slave = pty.openpty()
            try:
                env = detached_env(KUMU_TTY=os.ttyname(slave))
                process = subprocess.Popen(
                    [script, "--custom-config", "value with spaces"],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    env=env,
                    preexec_fn=os.setsid,
                )
                output = read_pty(master)
                stdout, stderr = process.communicate(timeout=5)
            finally:
                os.close(slave)
                os.close(master)
            text = stdout.decode()
            self.assertIn("ORIGINAL=<--custom-config>", text)
            self.assertIn("ORIGINAL=<value with spaces>", text)
            self.assertIn("\x1b]777;notify;codex;需要你确认操作权限\x07", output)
            self.assertEqual((process.returncode, stderr), (0, b""))

    def test_agy_hook_payloads_handle_session_cancel_and_error(self):
        cases = [
            (
                "session",
                {"conversationId": "conv-123"},
                "\x1b]777;notify;agy;kumu-session:conv-123\x07",
            ),
            (
                "start",
                {"conversationId": "conv-123", "lastUserInput": "Fix sidebar status"},
                "\x1b]777;notify;agy;kumu-session:conv-123\x07"
                "\x1b]777;notify;agy;kumu-title:prompt:Fix sidebar status\x07"
                "\x1b]777;notify;agy;kumu-status:running\x07",
            ),
            (
                "complete",
                {"conversationId": "conv-123", "terminationReason": "USER_CANCELED"},
                "\x1b]777;notify;agy;kumu-session:conv-123\x07"
                "\x1b]777;notify;agy;kumu-status:idle\x07",
            ),
            (
                "complete",
                {"conversationId": "conv-123", "terminationReason": "ERROR"},
                "\x1b]777;notify;agy;kumu-session:conv-123\x07"
                "\x1b]777;notify;agy;kumu-status:failed\x07"
                "\x1b]777;notify;agy;任务执行失败\x07",
            ),
            (
                "complete",
                {"conversationId": "conv-123", "terminationReason": "NO_TOOL_CALL"},
                "\x1b]777;notify;agy;kumu-session:conv-123\x07"
                "\x1b]777;notify;agy;kumu-status:completed\x07"
                "\x1b]777;notify;agy;本轮任务已结束\x07",
            ),
        ]
        for event, payload, expected in cases:
            with self.subTest(event=event, payload=payload):
                master, slave = pty.openpty()
                try:
                    env = detached_env(KUMU_TTY=os.ttyname(slave))
                    process = subprocess.Popen(
                        [BINARY, "--kumu-notify", "agy", event],
                        env=env,
                        stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        preexec_fn=os.setsid,
                    )
                    process.stdin.write(json.dumps(payload).encode())
                    process.stdin.close()
                    output = read_pty(master)
                    stdout, stderr = process.communicate(timeout=5)
                finally:
                    os.close(slave)
                    os.close(master)
                self.assertEqual((process.returncode, stdout, stderr), (0, b"", b""))
                self.assertEqual(output, expected)

    def test_start_and_resume_hooks_emit_silent_status_and_prompt_title(self):
        process = subprocess.run(
            [BINARY, "--kumu-notify", "claude", "start"],
            env=detached_env(),
            input=json.dumps({"session_id": "s1", "prompt": "Implement status\nmore"}).encode(),
            capture_output=True,
            timeout=5,
            preexec_fn=os.setsid,
        )
        self.assertEqual((process.returncode, process.stderr), (0, b""))
        sequence = json.loads(process.stdout)["terminalSequence"]
        self.assertEqual(
            sequence,
            "\x1b]777;notify;claude;kumu-session:s1\x07"
            "\x1b]777;notify;claude;kumu-title:prompt:Implement status\x07"
            "\x1b]777;notify;claude;kumu-status:running\x07",
        )

        resume = run_hook(["claude", "resume"], detached_env())
        self.assertEqual((resume.returncode, resume.stderr), (0, b""))
        self.assertEqual(
            json.loads(resume.stdout)["terminalSequence"],
            "\x1b]777;notify;claude;kumu-status:resume\x07",
        )

    def test_callbacks_are_silent_outside_kumu(self):
        for agent in ("codex", "claude", "grok", "cursor", "agy"):
            env = detached_env()
            env.pop("KUMU_TERMINAL")
            result = run_hook([agent, "complete"], env)
            self.assertEqual((result.returncode, result.stdout, result.stderr), (0, b"", b""))


if __name__ == "__main__":
    unittest.main(verbosity=2)
