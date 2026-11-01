//! The master pidfile: written at start, removed on drop.

use std::io;
use std::path::{Path, PathBuf};

pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    pub fn write(path: &Path) -> io::Result<PidFile> {
        std::fs::write(path, format!("{}\n", std::process::id()))?;
        Ok(PidFile {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
