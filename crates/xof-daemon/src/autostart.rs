//! Optional start-at-login (design §3): per-user, no admin rights.
//! Windows: HKCU Run key. Linux: systemd user unit. macOS: LaunchAgent.

use std::path::{Path, PathBuf};
use std::process::Command;

const NAME: &str = "ButterSat";

fn daemon_exe() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

/// Make the login registration match `enabled`.
pub fn apply(enabled: bool) -> anyhow::Result<()> {
    let exe = daemon_exe()?;
    if enabled {
        enable(&exe)
    } else {
        disable()
    }
}

#[cfg(windows)]
fn enable(exe: &Path) -> anyhow::Result<()> {
    let value = format!("\"{}\"", exe.display());
    let st = Command::new("reg")
        .args(["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", NAME, "/t", "REG_SZ", "/d"])
        .arg(&value)
        .arg("/f")
        .output()?;
    anyhow::ensure!(st.status.success(), "reg add failed: {}", String::from_utf8_lossy(&st.stderr));
    Ok(())
}

#[cfg(windows)]
fn disable() -> anyhow::Result<()> {
    let _ = Command::new("reg")
        .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", NAME, "/f"])
        .output()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn unit_path() -> anyhow::Result<PathBuf> {
    Ok(dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?.join("systemd/user/buttersat.service"))
}

#[cfg(target_os = "linux")]
fn enable(exe: &Path) -> anyhow::Result<()> {
    let p = unit_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(
        &p,
        format!(
            "[Unit]\nDescription=ButterSat scenery streaming daemon\n\n[Service]\nExecStart=\"{}\"\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
            exe.display()
        ),
    )?;
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    let _ = Command::new("systemctl").args(["--user", "enable", "buttersat.service"]).output();
    Ok(())
}

#[cfg(target_os = "linux")]
fn disable() -> anyhow::Result<()> {
    let _ = Command::new("systemctl").args(["--user", "disable", "buttersat.service"]).output();
    let p = unit_path()?;
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn plist_path() -> anyhow::Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home dir"))?
        .join("Library/LaunchAgents/org.buttersat.daemon.plist"))
}

#[cfg(target_os = "macos")]
fn enable(exe: &Path) -> anyhow::Result<()> {
    let p = plist_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(
        &p,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>org.buttersat.daemon</string>\n<key>ProgramArguments</key><array><string>{}</string></array>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
            exe.display()
        ),
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn disable() -> anyhow::Result<()> {
    let p = plist_path()?;
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn enable(_exe: &Path) -> anyhow::Result<()> {
    anyhow::bail!("start at login is not supported on this platform")
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn disable() -> anyhow::Result<()> {
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: &Path) {}
