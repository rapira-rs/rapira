//! The pidfile identifies the server until it exits.

use std::io;
use std::path::{Path, PathBuf};

pub struct PidFile {
    path: PathBuf,
    _cleanup: rapira_sapi::exit::CleanupGuard,
}

impl PidFile {
    pub fn write(path: &Path) -> io::Result<PidFile> {
        std::fs::write(path, format!("{}\n", std::process::id()))?;
        let owned = path.to_path_buf();
        let cleanup_path = owned.clone();
        let cleanup = rapira_sapi::exit::register(move || {
            let _ = std::fs::remove_file(&cleanup_path);
        });
        Ok(PidFile {
            path: owned,
            _cleanup: cleanup,
        })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
