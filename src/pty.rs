//! ConPTY wrapper (via `portable-pty`): launches the shell, keeps the master
//! side to forward keystrokes and resize, and hands the read side to the caller
//! to run the parser on its own thread.

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    _child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Open a PTY, spawn `shell` with `args`, and return the handle plus the
    /// read side.
    pub fn spawn(
        shell: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        banner_path: Option<&str>,
    ) -> Result<(Self, Box<dyn Read + Send>)> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.args(args);
        // Start in the user's home so the prompt looks sane.
        if let Ok(home) = std::env::var("USERPROFILE") {
            cmd.cwd(home);
        }
        // Advertise truecolor + a sane terminal type to apps that look.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Let the shell init detect betterm (enables the themed prompt).
        cmd.env("BETTERM", "1");
        // Path to the generated banner file the shell init prints at startup.
        if let Some(p) = banner_path {
            cmd.env("BETTERM_BANNER", p);
        }

        let child = pair.slave.spawn_command(cmd)?;
        // Slave handle is no longer needed in this process once spawned.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        Ok((
            Self {
                master: pair.master,
                writer,
                _child: child,
            },
            reader,
        ))
    }

    /// Send raw bytes to the shell (already encoded as a key sequence).
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}
