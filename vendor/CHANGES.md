# Changes to vendored code

## alacritty_terminal

`alacritty_terminal/` is [Alacritty](https://github.com/alacritty/alacritty)'s
terminal crate, version 0.26.0 (tag v0.17.0, commit `94e7c88`), under the Apache
License 2.0 (`alacritty_terminal/LICENSE-APACHE`). Files changed or added for Basalt
say so in their first line. The changes:

1. `tty::Options.clear_environment` (Windows) lets a session use only the environment
   it is given; by default the environment is inherited as upstream.
2. Windows command lines quote the executable path, so paths with spaces work.
3. An explicitly empty environment becomes a double-NUL block.
4. Duplicate-variable errors name only the key, not its value.
5. `tty::Options.manage_process_tree` lets the embedding app own a Windows job.
6. `tty/windows/control.rs` (added): owned process and job handles; a child is
   assigned to a kill-on-close job before it resumes.
7. ConPTY creation validates its inputs, reports OS errors and owns its handles, the
   DLL and the attribute list; closing can finish while output is still read.
8. The child watcher owns its callback context and covers an exit that happens before
   it is registered.
9. The blocking reader tells a pause in output from its end, and can drain output
   while cleaning up.
10. The event loop spawns fallibly, checks resizes, records exit and error state,
    bounds pending input and drains the last output.
11. `WindowSize` implements the core dimensions.
12. After a read fairness yield the event loop schedules a continuation, so buffered
    data is never left unread.
13. A side-by-side ConPTY DLL (with its OpenConsole host) can be selected by absolute
    path; `None` uses the Windows API.
14. Input is bounded to 1 MiB, resizes coalesce to the latest, and input after close
    is refused.
15. Windows process control exposes owned handles, kill-on-close jobs created before
    a child exists, timed waits, and the argument-quoting helper
    (`escaped_command_line`).
16. `term/progress.rs` (added) keeps the last progress report (`OSC 9;4`), which the
    parser drops; `Term::scan_progress` and `Term::progress` expose it, and the event
    loop scans output before parsing it.

## cmd-escaping

`cmd-escaping/` holds the license (MIT) of
[node-cross-spawn](https://github.com/moxystudio/node-cross-spawn), whose batch-file
argument escaping `core/src/system/process/command.rs` adapts.
