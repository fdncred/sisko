# Full Nushell session, not a terminal

Sisko hosts a complete Nushell Session (`config.nu`, env, plugins, `def`/`use`, external commands) inside a GPUI workbench. It is not a PTY or terminal emulator: interactive TUI programs are out of v1, and their captured stdout/stderr become a Result. A sandbox (no externals) would be a weaker daily shell; a graphical terminal would fight the table UI.
