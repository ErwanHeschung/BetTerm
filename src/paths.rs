//! Filesystem locations betterm writes to at runtime.
//!
//! Everything lands in `%LOCALAPPDATA%\betterm` (falling back to the system temp
//! dir when the variable is missing). The directory is resolved once and cached,
//! since it's hit on every log line and a couple of times at startup.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `%LOCALAPPDATA%\betterm`, created on first use. Returns `None` only if the
/// directory can't be created (e.g. permissions), in which case callers skip the
/// side effect rather than crash.
pub fn data_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("betterm");
        std::fs::create_dir_all(&dir).ok().map(|_| dir)
    })
    .as_deref()
}

/// Write `contents` to `<data_dir>/<name>` and return the resulting path.
pub fn write_data_file(name: &str, contents: &str) -> Option<PathBuf> {
    let path = data_dir()?.join(name);
    std::fs::write(&path, contents).ok()?;
    Some(path)
}
