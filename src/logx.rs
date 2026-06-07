//! Tiny logger. Because the app runs as a Windows GUI subsystem binary, stderr
//! is detached and invisible — so we also append to a log file the user (and we)
//! can read: `%LOCALAPPDATA%\betterm\betterm.log`.

use std::fs::OpenOptions;
use std::io::Write;

pub fn log(msg: &str) {
    eprintln!("[betterm] {msg}");
    let Some(dir) = crate::paths::data_dir() else {
        return;
    };
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("betterm.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}
