// Copied from Rubiks IDE (a Zed fork), crates/gpui_util/src/lib.rs (re-exported as util::command), at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

use std::ffi::OsStr;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000_u32;

#[cfg(target_os = "windows")]
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(target_os = "windows"))]
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    std::process::Command::new(program)
}
