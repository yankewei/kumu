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

pub fn open(program: &str, args: &[String], cwd: &Path, size: PtySize) -> std::io::Result<Pty> {
    let pair = native_pty_system()
        .openpty(size)
        .map_err(std::io::Error::other)?;
    let mut cmd = CommandBuilder::new(program);
    for arg in args {
        cmd.arg(arg);
    }
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
