//! A pseudoterminal and the process attached to its slave side.

use std::io::{Read, Write};
use std::path::Path;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub struct Pty {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

pub fn open(
    program: &str,
    args: &[String],
    cwd: &Path,
    size: PtySize,
    pane: Option<u64>,
) -> std::io::Result<Pty> {
    let pair = native_pty_system()
        .openpty(size)
        .map_err(std::io::Error::other)?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    // A Finder-launched app has launchd's environment, so an agent started
    // directly would miss what the user's shell exports (PATH, API keys).
    // Lay the captured login-shell environment under the pane's own values.
    // A pane that runs the login shell needs no capture: it reads the same
    // startup files itself, and applying it here would only duplicate what
    // the profile prepends to PATH.
    if !is_login_shell(program, args) {
        super::shell_env::apply(&mut cmd);
    }
    cmd.env("KUMU_TERMINAL", "1");
    // Which pane this process runs in. An agent inside it uses the id to ask
    // the app about its neighbours ("read the pane on my right").
    if let Some(pane) = pane {
        cmd.env("KUMU_PANE", pane.to_string());
    } else {
        cmd.env_remove("KUMU_PANE");
    }
    if let Ok(binary) = std::env::current_exe() {
        cmd.env("KUMU_APP_BINARY", binary);
    } else {
        cmd.env_remove("KUMU_APP_BINARY");
    }
    // Which Kumu started this pane. Another instance can serve the same socket
    // path, so a client asks for the app it actually belongs to.
    cmd.env(crate::control::PID_ENV, std::process::id().to_string());
    // Agent hooks run without a controlling terminal (Claude/Codex detach
    // them), so they cannot rely on `/dev/tty`. The slave's device path is the
    // channel back to this terminal's VT.
    if let Some(tty) = pair.master.tty_name() {
        cmd.env("KUMU_TTY", tty);
    }
    cmd.env("TERM_PROGRAM", "kumu");
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(std::io::Error::other)?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(std::io::Error::other)?;
    let writer = pair.master.take_writer().map_err(std::io::Error::other)?;
    Ok(Pty {
        master: pair.master,
        child,
        reader,
        writer,
    })
}

/// Whether this launch is the login shell a shell pane runs. That shell reads
/// the same startup files the environment capture read, so it does not need
/// the captured environment laid over it.
fn is_login_shell(program: &str, args: &[String]) -> bool {
    let (shell, shell_args) = super::default_shell();
    program == shell.as_str() && args == shell_args.as_slice()
}
