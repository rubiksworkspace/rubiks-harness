//! Bundled theme definitions (pure data, the product theme files verbatim), shared by the TUI's
//! theme loader (pa-tui) and the session HTML exporter (pa-core); a theme's `export` section
//! carries the export page's background overrides. Only the built-ins are data here; custom theme
//! discovery and behavior belong to the consuming crates.

/// The bundled theme names, in builtin resolution order.
pub const BUILTIN_THEME_NAMES: [&str; 5] = ["prime", "dark", "light", "rubiks", "rubiks-light"];

/// The `prime` theme file (Prime Agent's default, dark-first).
pub const PRIME_THEME_JSON: &str = include_str!("../themes/prime.json");
pub const DARK_THEME_JSON: &str = include_str!("../themes/dark.json");
pub const LIGHT_THEME_JSON: &str = include_str!("../themes/light.json");
/// The Rubiks themes: the Cube design system palette (paper and ink, blue,
/// red and yellow stickers), dark and light.
pub const RUBIKS_THEME_JSON: &str = include_str!("../themes/rubiks.json");
pub const RUBIKS_LIGHT_THEME_JSON: &str = include_str!("../themes/rubiks-light.json");

/// The bundled theme file for `name`, when `name` is a built-in theme.
#[must_use]
pub fn builtin_theme_json(name: &str) -> Option<&'static str> {
    match name {
        "prime" => Some(PRIME_THEME_JSON),
        "dark" => Some(DARK_THEME_JSON),
        "light" => Some(LIGHT_THEME_JSON),
        "rubiks" => Some(RUBIKS_THEME_JSON),
        "rubiks-light" => Some(RUBIKS_LIGHT_THEME_JSON),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled files parse as JSON objects with a name and colors, each
    /// advertising the theme name it is looked up by.
    #[test]
    fn builtin_theme_files_parse_and_match_their_names() {
        for name in BUILTIN_THEME_NAMES {
            let raw = builtin_theme_json(name).unwrap();
            let value: serde_json::Value = serde_json::from_str(raw).expect("valid JSON");
            assert_eq!(value["name"], *name);
            assert!(value["colors"].is_object(), "{name} has colors");
        }
    }
}
