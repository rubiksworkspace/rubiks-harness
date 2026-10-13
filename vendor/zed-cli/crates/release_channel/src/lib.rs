//! Provides constructs for the Zed app version and release channel.

#![deny(missing_docs)]

// Copied from Rubiks IDE (a Zed fork), crates/release_channel/src/lib.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

use std::{env, str::FromStr, sync::LazyLock};

const ZED_DOCS_URL: &str = "https://zed.dev/docs";

/// stable | dev | nightly | preview
pub static RELEASE_CHANNEL_NAME: LazyLock<String> = LazyLock::new(|| {
    if cfg!(debug_assertions) {
        env::var("ZED_RELEASE_CHANNEL").unwrap_or_else(|_| compile_time_release_channel_name())
    } else {
        compile_time_release_channel_name()
    }
});

/// When a crate in zed is used as a dependency that uses the `crane` nix
/// library, it vendors each crate separately and builds it in isolation, which
/// makes the `include_str!` fail.
///
/// The build script checks for `$ZED_RELEASE_CHANNEL` and emits the `cfg`
#[cfg(__do_not_set_zed_release_channel)]
fn compile_time_release_channel_name() -> String {
    env!("ZED_RELEASE_CHANNEL").trim().to_string()
}

#[cfg(not(__do_not_set_zed_release_channel))]
fn compile_time_release_channel_name() -> String {
    include_str!("../../zed/RELEASE_CHANNEL").trim().to_string()
}

#[doc(hidden)]
pub static RELEASE_CHANNEL: LazyLock<ReleaseChannel> =
    LazyLock::new(|| match ReleaseChannel::from_str(&RELEASE_CHANNEL_NAME) {
        Ok(channel) => channel,
        _ => panic!("invalid release channel {}", *RELEASE_CHANNEL_NAME),
    });
/// The app identifier for the current release channel, Windows only.
#[cfg(target_os = "windows")]
pub fn app_identifier() -> &'static str {
    match *RELEASE_CHANNEL {
        ReleaseChannel::Dev => "Zed-Editor-Dev",
        ReleaseChannel::Nightly => "Zed-Editor-Nightly",
        ReleaseChannel::Preview => "Zed-Editor-Preview",
        ReleaseChannel::Stable => "Zed-Editor-Stable",
    }
}

/// A Zed release channel.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum ReleaseChannel {
    /// The development release channel.
    ///
    /// Used for local debug builds of Zed.
    #[default]
    Dev,

    /// The Nightly release channel.
    Nightly,

    /// The Preview release channel.
    Preview,

    /// The Stable release channel.
    Stable,
}

impl ReleaseChannel {
    /// All release channels.
    pub const ALL: [ReleaseChannel; 4] = [
        ReleaseChannel::Dev,
        ReleaseChannel::Nightly,
        ReleaseChannel::Preview,
        ReleaseChannel::Stable,
    ];

    /// Returns whether we want to poll for updates for this [`ReleaseChannel`]
    pub fn poll_for_updates(&self) -> bool {
        !matches!(self, ReleaseChannel::Dev)
    }

    /// Returns the display name for this [`ReleaseChannel`].
    pub fn display_name(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "Rubiks IDE Dev",
            ReleaseChannel::Nightly => "Rubiks IDE Nightly",
            ReleaseChannel::Preview => "Rubiks IDE Preview",
            ReleaseChannel::Stable => "Rubiks IDE",
        }
    }

    /// Returns the programmatic name for this [`ReleaseChannel`].
    pub fn dev_name(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "dev",
            ReleaseChannel::Nightly => "nightly",
            ReleaseChannel::Preview => "preview",
            ReleaseChannel::Stable => "stable",
        }
    }

    /// Returns the application ID that's used by Wayland as application ID
    /// and WM_CLASS on X11.
    /// This also has to match the bundle identifier for Zed on macOS.
    pub fn app_id(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "dev.zed.Zed-Dev",
            ReleaseChannel::Nightly => "dev.zed.Zed-Nightly",
            ReleaseChannel::Preview => "dev.zed.Zed-Preview",
            // Distinct from Zed's "dev.zed.Zed", so macOS never mistakes one app for the other.
            ReleaseChannel::Stable => "com.rubiksworkspace.ide",
        }
    }

    /// Returns the query parameter for this [`ReleaseChannel`].
    pub fn release_query_param(&self) -> Option<&'static str> {
        match self {
            Self::Dev => None,
            Self::Nightly => Some("nightly=1"),
            Self::Preview => Some("preview=1"),
            Self::Stable => None,
        }
    }

    /// Returns the Zed docs URL for this [`ReleaseChannel`] for the given
    /// `slug`.
    pub fn docs_url(&self, slug: &str) -> String {
        let channel_path_segment = match self {
            Self::Dev | Self::Nightly => Some("nightly"),
            Self::Preview => Some("preview"),
            Self::Stable => None,
        };

        match channel_path_segment {
            Some(channel) if slug.is_empty() => format!("{ZED_DOCS_URL}/{channel}"),
            Some(channel) => format!("{ZED_DOCS_URL}/{channel}/{slug}"),
            None if slug.is_empty() => ZED_DOCS_URL.to_string(),
            None => format!("{ZED_DOCS_URL}/{slug}"),
        }
    }
}

/// Error indicating that release channel string does not match any known release channel names.
#[derive(Copy, Clone, Debug, Hash, PartialEq)]
pub struct InvalidReleaseChannel;

impl FromStr for ReleaseChannel {
    type Err = InvalidReleaseChannel;

    fn from_str(channel: &str) -> Result<Self, Self::Err> {
        Ok(match channel {
            "dev" => ReleaseChannel::Dev,
            "nightly" => ReleaseChannel::Nightly,
            "preview" => ReleaseChannel::Preview,
            "stable" => ReleaseChannel::Stable,
            _ => return Err(InvalidReleaseChannel),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ReleaseChannel;

    #[test]
    fn test_docs_url_for_release_channel() {
        assert_eq!(
            ReleaseChannel::Dev.docs_url("settings"),
            "https://zed.dev/docs/nightly/settings"
        );
        assert_eq!(
            ReleaseChannel::Nightly.docs_url("settings"),
            "https://zed.dev/docs/nightly/settings"
        );
        assert_eq!(
            ReleaseChannel::Preview.docs_url("settings"),
            "https://zed.dev/docs/preview/settings"
        );
        assert_eq!(
            ReleaseChannel::Stable.docs_url("settings"),
            "https://zed.dev/docs/settings"
        );
    }
}
