//! Short Unix socket paths for one data root. Nondefault roots get their own
//! directory so test and side-by-side daemons cannot control each other.

use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub directory: PathBuf,
}

impl Paths {
    pub fn for_data(data: &Path) -> Result<Self> {
        std::fs::create_dir_all(data)?;
        let data = data.canonicalize()?;
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let default = if cfg!(target_os = "macos") {
            PathBuf::from(home).join("Library/Application Support/deck")
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(home).join(".local/share"))
                .join("deck")
        };
        // SAFETY: getuid has no arguments or pointers.
        let uid = unsafe { libc::getuid() };
        let mut directory = std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join("deck"))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/deck-{uid}")));
        if data != default.canonicalize().unwrap_or(default) {
            // FNV-1a is stable across processes and Rust versions.
            use std::os::unix::ffi::OsStrExt as _;
            let hash = data
                .as_os_str()
                .as_bytes()
                .iter()
                .fold(0xcbf29ce484222325u64, |n, b| (n ^ u64::from(*b)).wrapping_mul(0x100000001b3));
            directory.push(format!("{hash:x}"));
        }
        Ok(Self { directory })
    }

    pub fn control(&self) -> PathBuf {
        self.directory.join("deckd.sock")
    }
    pub fn task(&self, id: &str) -> PathBuf {
        self.directory.join("tasks").join(format!("{id}.sock"))
    }

    pub(crate) fn prepare(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(self.directory.join("tasks"))?;
        std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
}
