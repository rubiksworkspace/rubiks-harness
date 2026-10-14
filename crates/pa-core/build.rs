//! Records the commit the harness is built from (`PA_HARNESS_COMMIT`), which
//! the research log stamps on every record as part of the harness version.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn main() {
    let commit = std::env::var("PA_HARNESS_COMMIT")
        .ok()
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=PA_HARNESS_COMMIT={commit}");
    println!("cargo:rerun-if-env-changed=PA_HARNESS_COMMIT");
    // A new commit or a branch switch rebuilds with the new id; ordinary
    // edits do not.
    // Only existing paths: a missing one would rerun this on every build.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for path in ["HEAD", "refs/heads", "packed-refs"] {
            let path = std::path::Path::new(&git_dir).join(path);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}
