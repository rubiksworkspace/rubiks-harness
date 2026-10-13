// Copied from Rubiks IDE (a Zed fork), crates/util/src/paths.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::mem;
use std::path::StripPrefixError;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

/// Returns the path to the user's home directory.
#[cfg(not(target_family = "wasm"))]
pub fn home_dir() -> &'static PathBuf {
    static HOME_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME_DIR.get_or_init(|| {
        if cfg!(any(test, feature = "test-support")) {
            if cfg!(target_os = "macos") {
                PathBuf::from("/Users/zed")
            } else if cfg!(target_os = "windows") {
                PathBuf::from("C:\\Users\\zed")
            } else {
                PathBuf::from("/home/zed")
            }
        } else {
            dirs::home_dir().expect("failed to determine home directory")
        }
    })
}

/// In memory, this is identical to `Path`. On non-Windows conversions to this type are no-ops. On
/// windows, these conversions sanitize UNC paths by removing the `\\\\?\\` prefix.
#[derive(Eq, PartialEq, Hash, Ord, PartialOrd)]
#[repr(transparent)]
pub struct SanitizedPath(Path);

impl SanitizedPath {
    pub fn new<T: AsRef<Path> + ?Sized>(path: &T) -> &Self {
        #[cfg(not(target_os = "windows"))]
        return Self::unchecked_new(path.as_ref());

        #[cfg(target_os = "windows")]
        return Self::unchecked_new(dunce::simplified(path.as_ref()));
    }

    pub fn unchecked_new<T: AsRef<Path> + ?Sized>(path: &T) -> &Self {
        // safe because `Path` and `SanitizedPath` have the same repr and Drop impl
        unsafe { mem::transmute::<&Path, &Self>(path.as_ref()) }
    }

    pub fn from_arc(path: Arc<Path>) -> Arc<Self> {
        // safe because `Path` and `SanitizedPath` have the same repr and Drop impl
        #[cfg(not(target_os = "windows"))]
        return unsafe { mem::transmute::<Arc<Path>, Arc<Self>>(path) };

        #[cfg(target_os = "windows")]
        {
            let simplified = dunce::simplified(path.as_ref());
            if simplified == path.as_ref() {
                // safe because `Path` and `SanitizedPath` have the same repr and Drop impl
                unsafe { mem::transmute::<Arc<Path>, Arc<Self>>(path) }
            } else {
                Self::unchecked_new(simplified).into()
            }
        }
    }

    pub fn new_arc<T: AsRef<Path> + ?Sized>(path: &T) -> Arc<Self> {
        Self::new(path).into()
    }

    pub fn cast_arc(path: Arc<Self>) -> Arc<Path> {
        // safe because `Path` and `SanitizedPath` have the same repr and Drop impl
        unsafe { mem::transmute::<Arc<Self>, Arc<Path>>(path) }
    }

    pub fn cast_arc_ref(path: &Arc<Self>) -> &Arc<Path> {
        // safe because `Path` and `SanitizedPath` have the same repr and Drop impl
        unsafe { mem::transmute::<&Arc<Self>, &Arc<Path>>(path) }
    }

    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn file_name(&self) -> Option<&std::ffi::OsStr> {
        self.0.file_name()
    }

    pub fn extension(&self) -> Option<&std::ffi::OsStr> {
        self.0.extension()
    }

    pub fn join<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        self.0.join(path)
    }

    pub fn parent(&self) -> Option<&Self> {
        self.0.parent().map(Self::unchecked_new)
    }

    pub fn strip_prefix(&self, base: &Self) -> Result<&Path, StripPrefixError> {
        self.0.strip_prefix(base.as_path())
    }

    pub fn to_str(&self) -> Option<&str> {
        self.0.to_str()
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.0.to_path_buf()
    }
}

impl std::fmt::Debug for SanitizedPath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, formatter)
    }
}

impl Display for SanitizedPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl From<&SanitizedPath> for Arc<SanitizedPath> {
    fn from(sanitized_path: &SanitizedPath) -> Self {
        let path: Arc<Path> = sanitized_path.0.into();
        // safe because `Path` and `SanitizedPath` have the same repr and Drop impl
        unsafe { mem::transmute(path) }
    }
}

impl From<&SanitizedPath> for PathBuf {
    fn from(sanitized_path: &SanitizedPath) -> Self {
        sanitized_path.as_path().into()
    }
}

impl AsRef<Path> for SanitizedPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A delimiter to use in `path_query:row_number:column_number` strings parsing.
pub const FILE_ROW_COLUMN_DELIMITER: char = ':';

const ROW_COL_CAPTURE_REGEX: &str = r"(?xs)
    ([^\(]+)\:(?:
        \((\d+)[,:](\d+)\) # filename:(row,column), filename:(row:column)
        |
        \((\d+)\)()     # filename:(row)
    )
    |
    ([^\(]+)(?:
        \((\d+)[,:](\d+)\) # filename(row,column), filename(row:column)
        |
        \((\d+)\)()     # filename(row)
    )
    \:*$
    |
    (.+?)(?:
        \:+(\d+)\:(\d+)\:*$  # filename:row:column
        |
        \:+(\d+)\:*()$       # filename:row
        |
        \:+()()$
    )";

/// A representation of a path-like string with optional row and column numbers.
/// Matching values example: `te`, `test.rs:22`, `te:22:5`, `test.c(22)`, `test.c(22,5)`etc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct PathWithPosition {
    pub path: PathBuf,
    pub row: Option<u32>,
    // Absent if row is absent.
    pub column: Option<u32>,
}

impl PathWithPosition {
    /// Returns a PathWithPosition from a path.
    pub fn from_path(path: PathBuf) -> Self {
        Self {
            path,
            row: None,
            column: None,
        }
    }

    /// Parses a string that possibly has `:row:column` or `(row, column)` suffix.
    /// Parenthesis format is used by [MSBuild](https://learn.microsoft.com/en-us/visualstudio/msbuild/msbuild-diagnostic-format-for-tasks) compatible tools
    /// Ignores trailing `:`s, so `test.rs:22:` is parsed as `test.rs:22`.
    /// If the suffix parsing fails, the whole string is parsed as a path.
    ///
    /// Be mindful that `test_file:10:1:` is a valid posix filename.
    /// `PathWithPosition` class assumes that the ending position-like suffix is **not** part of the filename.
    ///
    /// # Examples
    ///
    /// ```
    /// # use util::paths::PathWithPosition;
    /// # use std::path::PathBuf;
    /// assert_eq!(PathWithPosition::parse_str("test_file"), PathWithPosition {
    ///     path: PathBuf::from("test_file"),
    ///     row: None,
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file:10"), PathWithPosition {
    ///     path: PathBuf::from("test_file"),
    ///     row: Some(10),
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs"),
    ///     row: None,
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs:1"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs"),
    ///     row: Some(1),
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs:1:2"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs"),
    ///     row: Some(1),
    ///     column: Some(2),
    /// });
    /// ```
    ///
    /// # Expected parsing results when encounter ill-formatted inputs.
    /// ```
    /// # use util::paths::PathWithPosition;
    /// # use std::path::PathBuf;
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs:a"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs:a"),
    ///     row: None,
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs:a:b"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs:a:b"),
    ///     row: None,
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs"),
    ///     row: None,
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs::1"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs"),
    ///     row: Some(1),
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs:1::"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs"),
    ///     row: Some(1),
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs::1:2"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs"),
    ///     row: Some(1),
    ///     column: Some(2),
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs:1::2"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs:1"),
    ///     row: Some(2),
    ///     column: None,
    /// });
    /// assert_eq!(PathWithPosition::parse_str("test_file.rs:1:2:3"), PathWithPosition {
    ///     path: PathBuf::from("test_file.rs:1"),
    ///     row: Some(2),
    ///     column: Some(3),
    /// });
    /// ```
    pub fn parse_str(s: &str) -> Self {
        let trimmed = s.trim();
        let path = Path::new(trimmed);
        let Some(maybe_file_name_with_row_col) = path.file_name().unwrap_or_default().to_str()
        else {
            return Self {
                path: Path::new(s).to_path_buf(),
                row: None,
                column: None,
            };
        };
        if maybe_file_name_with_row_col.is_empty() {
            return Self {
                path: Path::new(s).to_path_buf(),
                row: None,
                column: None,
            };
        }

        // Let's avoid repeated init cost on this. It is subject to thread contention, but
        // so far this code isn't called from multiple hot paths. Getting contention here
        // in the future seems unlikely.
        static SUFFIX_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(ROW_COL_CAPTURE_REGEX).unwrap());
        match SUFFIX_RE
            .captures(maybe_file_name_with_row_col)
            .map(|caps| caps.extract())
        {
            Some((_, [file_name, maybe_row, maybe_column])) => {
                let row = maybe_row.parse::<u32>().ok();
                let column = maybe_column.parse::<u32>().ok();

                let (_, suffix) = trimmed.split_once(file_name).unwrap();
                let path_without_suffix = &trimmed[..trimmed.len() - suffix.len()];

                Self {
                    path: Path::new(path_without_suffix).to_path_buf(),
                    row,
                    column,
                }
            }
            None => {
                // The `ROW_COL_CAPTURE_REGEX` deals with separated digits only,
                // but in reality there could be `foo/bar.py:22:in` inputs which we want to match too.
                // The regex mentioned is not very extendable with "digit or random string" checks, so do this here instead.
                let delimiter = ':';
                let mut path_parts = s
                    .rsplitn(3, delimiter)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .fuse();
                let mut path_string = path_parts.next().expect("rsplitn should have the rest of the string as its last parameter that we reversed").to_owned();
                let mut row = None;
                let mut column = None;
                if let Some(maybe_row) = path_parts.next() {
                    if let Ok(parsed_row) = maybe_row.parse::<u32>() {
                        row = Some(parsed_row);
                        if let Some(parsed_column) = path_parts
                            .next()
                            .and_then(|maybe_col| maybe_col.parse::<u32>().ok())
                        {
                            column = Some(parsed_column);
                        }
                    } else {
                        path_string.push(delimiter);
                        path_string.push_str(maybe_row);
                    }
                }
                for split in path_parts {
                    path_string.push(delimiter);
                    path_string.push_str(split);
                }

                Self {
                    path: PathBuf::from(path_string),
                    row,
                    column,
                }
            }
        }
    }

    pub fn map_path<E>(
        self,
        mapping: impl FnOnce(PathBuf) -> Result<PathBuf, E>,
    ) -> Result<PathWithPosition, E> {
        Ok(PathWithPosition {
            path: mapping(self.path)?,
            row: self.row,
            column: self.column,
        })
    }

    pub fn to_string(&self, path_to_string: &dyn Fn(&PathBuf) -> String) -> String {
        let path_string = path_to_string(&self.path);
        if let Some(row) = self.row {
            if let Some(column) = self.column {
                format!("{path_string}:{row}:{column}")
            } else {
                format!("{path_string}:{row}")
            }
        } else {
            path_string
        }
    }
}
