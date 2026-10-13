// Copied from Rubiks IDE (a Zed fork), crates/util/src/test.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct TempTree {
    _temp_dir: TempDir,
    path: PathBuf,
}

impl TempTree {
    pub fn new(tree: serde_json::Value) -> Self {
        let dir = TempDir::new().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        write_tree(path.as_path(), tree);

        Self {
            _temp_dir: dir,
            path,
        }
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }
}

fn write_tree(path: &Path, tree: serde_json::Value) {
    use serde_json::Value;
    use std::fs;

    if let Value::Object(map) = tree {
        for (name, contents) in map {
            let mut path = PathBuf::from(path);
            path.push(name);
            match contents {
                Value::Object(_) => {
                    fs::create_dir(&path).unwrap();

                    #[cfg(not(target_family = "wasm"))]
                    #[allow(clippy::disallowed_methods)]
                    if path.file_name() == Some(OsStr::new(".git")) {
                        let output = std::process::Command::new("git")
                            .args(["init", "-b", "main"])
                            .current_dir(path.parent().unwrap())
                            .env("GIT_CONFIG_GLOBAL", "")
                            .env("GIT_CONFIG_SYSTEM", "")
                            .output()
                            .expect("failed to init git repo");
                        assert!(
                            output.status.success(),
                            "git init failed: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }

                    write_tree(&path, contents);
                }
                Value::Null => {
                    fs::create_dir(&path).unwrap();
                }
                Value::String(contents) => {
                    fs::write(&path, contents).unwrap();
                }
                _ => {
                    panic!("JSON object must contain only objects, strings, or null");
                }
            }
        }
    } else {
        panic!("You must pass a JSON object to this helper")
    }
}
