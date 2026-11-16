//! ButterSat's On/Off switch, shared by the app and `xof-cli on|off`.
//!
//! - **On**: persist `startup.enabled = true` and start the daemon, which
//!   mounts and adds or re-enables our `scenery_packs.ini` entry.
//! - **Off**: ask a running daemon to switch off ([`crate::method::TURN_OFF`]:
//!   persist, unmount, disable the ini entry, exit). With no daemon running,
//!   do the persistent parts here.
//!
//! `xof-cli shutdown` (used by the installer for upgrades) only stops the
//! daemon and leaves the switch alone.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{DaemonConfig, Dirs};
use crate::{method, xplane, Client};

/// Persist the switch in the config file (creating it if needed).
pub fn set_enabled(config_path: &Path, enabled: bool) -> anyhow::Result<DaemonConfig> {
    let mut cfg = DaemonConfig::load(config_path).unwrap_or_default();
    if cfg.startup.enabled != enabled {
        cfg.startup.enabled = enabled;
        cfg.save(config_path)?;
    }
    Ok(cfg)
}

/// The X-Plane installs whose `scenery_packs.ini` we manage: the configured
/// one, else every detected install.
pub fn managed_installs(cfg: &DaemonConfig) -> Vec<PathBuf> {
    if let Some(p) = &cfg.xplane.path {
        return vec![xplane::native_path(p)];
    }
    if std::env::var_os("XOF_HOME").is_some() {
        return Vec::new();
    }
    xplane::detect_installs()
}

/// `xof-daemon` next to `exe_dir` (the app's or CLI's own folder).
pub fn daemon_path(exe_dir: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "xof-daemon.exe" } else { "xof-daemon" };
    let p = exe_dir.join(name);
    p.exists().then_some(p)
}

/// Start the daemon detached so it outlives the caller.
pub fn spawn_daemon(path: &Path) -> anyhow::Result<()> {
    // Windows children inherit every inheritable handle, including our
    // stdout/stderr pipes: a long-lived daemon holding them makes whoever
    // reads our output (`xof-cli on | ...`, scripts) wait forever for EOF.
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
        let handles =
            [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()];
        for h in handles {
            if !h.is_null() {
                // SAFETY: our own standard handles; failure is harmless.
                let _ = unsafe { SetHandleInformation(HANDLE(h), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
            }
        }
    }
    let mut cmd = std::process::Command::new(path);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    Ok(())
}

/// Switch on: persist, then start the daemon (if not running) and wait until
/// it answers.
pub async fn turn_on(exe_dir: &Path) -> anyhow::Result<Client> {
    set_enabled(&Dirs::standard().config_file, true)?;
    if let Ok(c) = Client::connect().await {
        return Ok(c);
    }
    let path =
        daemon_path(exe_dir).ok_or_else(|| anyhow::anyhow!("xof-daemon not found next to {}", exe_dir.display()))?;
    spawn_daemon(&path)?;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(c) = Client::connect().await {
            return Ok(c);
        }
    }
    anyhow::bail!("xof-daemon did not start")
}

/// Switch off. Errors (without changing anything) while X-Plane is connected.
pub async fn turn_off() -> anyhow::Result<()> {
    if let Ok(c) = Client::connect().await {
        c.call::<_, serde_json::Value>(method::TURN_OFF, serde_json::Value::Null).await?;
        // Wait for the daemon to exit so a following "on" starts a fresh one.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if Client::connect().await.is_err() {
                return Ok(());
            }
        }
        return Ok(());
    }
    // No daemon: nothing is mounted; persist and disable the ini entry here.
    let cfg = set_enabled(&Dirs::standard().config_file, false)?;
    if cfg.mount.manage_scenery_packs {
        for xp in managed_installs(&cfg) {
            xplane::set_scenery_pack_enabled(&xp, false)?;
        }
    }
    Ok(())
}
