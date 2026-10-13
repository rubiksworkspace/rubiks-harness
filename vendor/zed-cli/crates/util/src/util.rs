// Copied from Rubiks IDE (a Zed fork), crates/util/src/util.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

pub mod command;
pub mod paths;
#[cfg(any(test, feature = "test-support"))]
pub mod test;

pub use util_macros::path;

/// Prevents execution of the application with root privileges on Unix systems.
///
/// This function checks if the current process is running with root privileges
/// and terminates the program with an error message unless explicitly allowed via the
/// `ZED_ALLOW_ROOT` environment variable.
#[cfg(unix)]
pub fn prevent_root_execution() {
    let is_root = nix::unistd::geteuid().is_root();
    let allow_root = std::env::var("ZED_ALLOW_ROOT").is_ok_and(|val| val == "true");

    if is_root && !allow_root {
        eprintln!(
            "\
Error: Running Zed as root or via sudo is unsupported.
       Doing so (even once) may subtly break things for all subsequent non-root usage of Zed.
       It is untested and not recommended, don't complain when things break.
       If you wish to proceed anyways, set `ZED_ALLOW_ROOT=true` in your environment."
        );
        std::process::exit(1);
    }
}
