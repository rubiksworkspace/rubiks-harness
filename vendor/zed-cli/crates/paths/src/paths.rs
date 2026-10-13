//! Paths to locations used by Zed.
// Copied from Rubiks IDE (a Zed fork), crates/paths/src/paths.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

use std::path::PathBuf;
use std::sync::OnceLock;
use util::paths::SanitizedPath;

pub use util::paths::home_dir;

/// The application name, used to derive platform-specific data, config, cache,
/// and state directory paths.
///
/// Forks should change this to avoid colliding with Zed's user data.
pub const APP_NAME: &str = "Zed";

/// Lowercased form of [`APP_NAME`], for use in XDG-style paths on
/// Linux/FreeBSD and the macOS `~/.config` fallback.
pub const APP_NAME_LOWERCASE: &str = {
    assert!(!APP_NAME.is_empty(), "APP_NAME must not be empty");
    assert!(APP_NAME.as_bytes().is_ascii(), "APP_NAME must be ASCII");
    const BYTES: [u8; APP_NAME.len()] = {
        let mut bytes = [0u8; APP_NAME.len()];
        let mut i = 0;
        while i < APP_NAME.len() {
            assert!(
                APP_NAME.as_bytes()[i] != b'/' && APP_NAME.as_bytes()[i] != b'\\',
                "APP_NAME must not contain path separators",
            );
            assert!(
                APP_NAME.as_bytes()[i] >= 0x20,
                "APP_NAME must not contain control characters"
            );
            bytes[i] = APP_NAME.as_bytes()[i];
            i += 1;
        }
        bytes.make_ascii_lowercase();
        bytes
    };
    match std::str::from_utf8(&BYTES) {
        Ok(s) => s,
        Err(_) => unreachable!(),
    }
};

/// A custom data directory override, set only by `set_custom_data_dir`.
/// This is used to override the default data directory location.
/// The directory will be created if it doesn't exist when set.
static CUSTOM_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The resolved data directory, combining custom override or platform defaults.
/// This is set once and cached for subsequent calls.
/// On macOS, this is `~/Library/Application Support/Zed`.
/// On Linux/FreeBSD, this is `$XDG_DATA_HOME/zed`.
/// On Windows, this is `%LOCALAPPDATA%\Zed`.
static CURRENT_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The resolved config directory, combining custom override or platform defaults.
/// This is set once and cached for subsequent calls.
/// On macOS, this is `~/.config/zed`.
/// On Linux/FreeBSD, this is `$XDG_CONFIG_HOME/zed`.
/// On Windows, this is `%APPDATA%\Zed`.
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Sets a custom directory for all user data, overriding the default data directory.
/// This function must be called before any other path operations that depend on the data directory.
/// The directory's path will be canonicalized to an absolute path by a blocking FS operation.
/// The directory will be created if it doesn't exist.
///
/// # Arguments
///
/// * `dir` - The path to use as the custom data directory. This will be used as the base
///   directory for all user data, including databases, extensions, and logs.
///
/// # Returns
///
/// A reference to the static `PathBuf` containing the custom data directory path.
///
/// # Panics
///
/// Panics if:
/// * Called after the data directory has been initialized (e.g., via `data_dir` or `config_dir`)
/// * The directory's path cannot be canonicalized to an absolute path
/// * The directory cannot be created
pub fn set_custom_data_dir(dir: &str) -> &'static PathBuf {
    if CURRENT_DATA_DIR.get().is_some() || CONFIG_DIR.get().is_some() {
        panic!("set_custom_data_dir called after data_dir or config_dir was initialized");
    }
    CUSTOM_DATA_DIR.get_or_init(|| {
        let path = PathBuf::from(dir);
        std::fs::create_dir_all(&path).expect("failed to create custom data directory");
        let canonicalized = path
            .canonicalize()
            .expect("failed to canonicalize custom data directory's path to an absolute path");
        // On Windows, `canonicalize` produces extended-length paths prefixed
        // with `\\?\`. Strip that prefix so downstream consumers (e.g.
        // Node.js language servers) that receive derived paths as arguments
        // don't choke on the verbatim syntax.
        SanitizedPath::new(&canonicalized).as_path().to_path_buf()
    })
}

/// Returns the path to the configuration directory used by Zed.
pub fn config_dir() -> &'static PathBuf {
    CONFIG_DIR.get_or_init(|| {
        if let Some(custom_dir) = CUSTOM_DATA_DIR.get() {
            custom_dir.join("config")
        } else if cfg!(target_os = "windows") {
            dirs::config_dir()
                .expect("failed to determine RoamingAppData directory")
                .join(APP_NAME)
        } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            if let Ok(flatpak_xdg_config) = std::env::var("FLATPAK_XDG_CONFIG_HOME") {
                flatpak_xdg_config.into()
            } else {
                dirs::config_dir().expect("failed to determine XDG_CONFIG_HOME directory")
            }
            .join(APP_NAME_LOWERCASE)
        } else {
            home_dir().join(".config").join(APP_NAME_LOWERCASE)
        }
    })
}

/// Returns the path to the data directory used by Zed.
pub fn data_dir() -> &'static PathBuf {
    CURRENT_DATA_DIR.get_or_init(|| {
        if let Some(custom_dir) = CUSTOM_DATA_DIR.get() {
            custom_dir.clone()
        } else if cfg!(target_os = "macos") {
            home_dir()
                .join("Library/Application Support")
                .join(APP_NAME)
        } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            if let Ok(flatpak_xdg_data) = std::env::var("FLATPAK_XDG_DATA_HOME") {
                flatpak_xdg_data.into()
            } else {
                dirs::data_local_dir().expect("failed to determine XDG_DATA_HOME directory")
            }
            .join(APP_NAME_LOWERCASE)
        } else if cfg!(target_os = "windows") {
            dirs::data_local_dir()
                .expect("failed to determine LocalAppData directory")
                .join(APP_NAME)
        } else {
            config_dir().clone() // Fallback
        }
    })
}
