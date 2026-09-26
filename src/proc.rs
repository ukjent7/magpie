// proc makes the commands magpie runs: the agents' own CLIs, the probes that
// ask them what they have, and the system's tools those read. Every one is
// made here rather than with std::process, so that on Windows none of them
// opens a window of its own.
//
// The desktop app on Windows can run with no console at all — opened from
// the taskbar, or started as a login item. Windows then gives every console
// program it starts (claude, codex, grok, devin, and the probes behind them)
// a console of its own, and with it a window, one for every run. So a child
// of a magpie with no console is started with CREATE_NO_WINDOW, which gives
// it a console without one. A magpie run in a terminal lets its children
// share that console as before, so Ctrl+C there reaches them too. Nothing
// magpie starts reads or writes the terminal itself: its output is piped
// back, which a hidden console doesn't change.

use std::process::Command as StdCommand;
use tokio::process::Command as AsyncCommand;

// CREATE_NO_WINDOW: a console for the child, but no window to show it in.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// command is std's Command, with no window of its own on Windows.
#[cfg(windows)]
pub(crate) fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> StdCommand {
    let mut command = StdCommand::new(program);
    if no_console() {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(not(windows))]
pub(crate) fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> StdCommand {
    StdCommand::new(program)
}

// async_command is tokio's, with the same care.
#[cfg(windows)]
pub(crate) fn async_command<S: AsRef<std::ffi::OsStr>>(program: S) -> AsyncCommand {
    let mut command = AsyncCommand::new(program);
    if no_console() {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(not(windows))]
pub(crate) fn async_command<S: AsRef<std::ffi::OsStr>>(program: S) -> AsyncCommand {
    AsyncCommand::new(program)
}

// no_console reports whether magpie has no console for a child to share. It
// is asked for every command, since a magpie started from a terminal takes
// that console over once it is under way.
#[cfg(windows)]
fn no_console() -> bool {
    use std::io::IsTerminal;
    !(std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
        || std::io::stderr().is_terminal())
}
