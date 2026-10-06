# Portside

Windows taskbar strip listing local dev servers: cycle through them, see how many there are, click one to open it in the default browser, middle-click one to stop it. It sits right of agent-usage-overlay and matches its look.

- Status: working. Same stack as agent-usage-overlay: Rust 2024, `windows-sys`, `win-taskbar-host` (pinned to a release tag in `Cargo.toml`) for hosting, Direct2D/DirectWrite in `src/directwrite.cpp` built with `cc`.
- Code: `servers.rs` reads the TCP listener table every second, looks up a process's name and working directory once per new PID and port, and stops a server (Ctrl+C in its console, then `taskkill`). `ui.rs` hosts the strip, draws it and the server menu, and handles input. `render.rs` wraps `directwrite.cpp`. `instance.rs` protects the running instance and handles installer start/stop requests.
- Design: `src/theme.rs` holds every size, color, font and icon, in DIPs. `design/spec.md` describes states and behavior by those names. Change them together.
- Font: Atkinson Hyperlegible Regular only, embedded from `assets/fonts/`, with its OFL beside it. Icons are Segoe Fluent Icons glyphs from Windows, never files. The one exception is the exe's icon, `assets/portside.ico`, which `build.rs` embeds through `src/portside.rc`, along with the version info that names the exe Portside, published by Leon Björklund.
- Install: `./packaging/package.ps1` builds the unsigned per-user Inno installer under `target/installer`. Inno owns files, startup, the Start menu shortcut and uninstall registration. Run it outside packaged app shells such as Codex, which can redirect LocalAppData and HKCU; verify the real user's installation. Right-click > Exit stops it; Move places it, and the host saves the position outside the installation directory.
- Checks: `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo build --release`. Then run the release exe and compare each state in `design/spec.md` on the real taskbar.
