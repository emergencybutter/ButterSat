//! xof-vfs: the virtual scenery filesystem.
//!
//! [`scenery::Scenery`] is the platform-independent tree; backends adapt it to
//! WinFsp (Windows) or FUSE (Linux, macOS via macFUSE).

pub mod process;
pub mod scenery;

#[cfg(windows)]
pub mod winfsp_backend;

#[cfg(unix)]
pub mod fuse_backend;

use std::path::Path;
use std::sync::Arc;

pub use scenery::{Node, Scenery, SceneryConfig};

/// A mounted filesystem; dropping it unmounts.
pub struct Mounted {
    #[cfg(windows)]
    _inner: winfsp_backend::Mount,
    #[cfg(unix)]
    _inner: fuse_backend::Mount,
}

/// Mount `scenery` at `mount_point` with the platform's backend.
pub fn mount(scenery: Arc<Scenery>, mount_point: &Path) -> anyhow::Result<Mounted> {
    #[cfg(windows)]
    {
        Ok(Mounted { _inner: winfsp_backend::Mount::new(scenery, mount_point)? })
    }
    #[cfg(unix)]
    {
        Ok(Mounted { _inner: fuse_backend::Mount::new(scenery, mount_point)? })
    }
}

/// Is the platform filesystem driver available (WinFsp / libfuse3 / macFUSE)?
pub fn driver_available() -> bool {
    #[cfg(windows)]
    {
        winfsp_backend::available()
    }
    #[cfg(unix)]
    {
        fuse_backend::available()
    }
}

/// Human-readable driver name for health checks.
pub fn driver_name() -> &'static str {
    if cfg!(windows) {
        "WinFsp"
    } else if cfg!(target_os = "macos") {
        "macFUSE"
    } else {
        "FUSE (libfuse3)"
    }
}
