// Copied from Rubiks IDE (a Zed fork), crates/util_macros/src/util_macros.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

use proc_macro::TokenStream;
use quote::quote;
use syn::{LitStr, parse_macro_input};

/// A macro used in tests for cross-platform path string literals in tests. On Windows it replaces
/// `/` with `\\` and adds `C:` to the beginning of absolute paths. On other platforms, the path is
/// returned unmodified.
///
/// # Example
/// ```rust
/// use util_macros::path;
///
/// let path = path!("/Users/user/file.txt");
/// #[cfg(target_os = "windows")]
/// assert_eq!(path, "C:\\Users\\user\\file.txt");
/// #[cfg(not(target_os = "windows"))]
/// assert_eq!(path, "/Users/user/file.txt");
/// ```
#[proc_macro]
pub fn path(input: TokenStream) -> TokenStream {
    let path = parse_macro_input!(input as LitStr);
    let mut path = path.value();

    #[cfg(target_os = "windows")]
    {
        path = path.replace("/", "\\");
        if path.starts_with("\\") {
            path = format!("C:{}", path);
        }
    }

    TokenStream::from(quote! {
        #path
    })
}
