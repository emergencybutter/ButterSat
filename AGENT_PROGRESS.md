2026-11-15T20:55:00Z Started release task on worktree ButterSat-wt-release (branch agent/release); reviewed common.md, release.md, design.md, bench.md, and existing build/installer configs.
2026-11-15T21:10:30Z Diagnosed CI failures on main (xof-core engine tests scheduling background upgrades for ReaderClass::Other and mock PNG re-encoding overhead); applied fix and verified locally (engine tests pass in 1.76s). Generated Tauri updater minisign keypair in xporthofs-build/keys/updater.key.
2026-11-15T21:17:00Z Committed and pushed CI fixes to origin/agent/release; implemented xof-cli uninstall (--remove-cache), per-user NSIS installer hooks, tauri-plugin-updater + UI update banner, nginx /updates/ location, and docs/release.md.
2026-11-15T21:30:30Z Built and signed ButterSat_0.1.0_x64-setup.exe (13,247,035 bytes, SHA-256 856ca69301e4c0dcd90247f59a5e5292677028313eb162379e1f33b9d3e1f8a2) + .sig and latest.json; verified isolated daemon/CLI install & uninstall dry runs.
2026-11-15T21:46:46Z Verified GitHub Actions CI run 43764318742 on agent/release passed all 5 jobs (test windows/ubuntu/macos, licences and advisories, fmt); wrote AGENT_REPORT.md.



