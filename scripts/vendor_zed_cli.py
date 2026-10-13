#!/usr/bin/env python3
"""Vendors Zed's CLI (as carried by Rubiks IDE) into vendor/zed-cli.

    python3 scripts/vendor_zed_cli.py <path to a Rubiks-IDE checkout>

The CLI's own sources are copied unchanged, in the same crates/ and script/
layout as the Zed repository so their relative paths keep working. The internal Zed crates it uses
(askpass, collections, net, paths, release_channel, util, util_macros) are
reduced to the items the CLI needs and keep their original crate names, so the
CLI's `use` paths resolve as-is. Rerun after updating Rubiks IDE; it stops with
an error when an upstream change no longer matches what it copies.
"""
import re
import shutil
import subprocess
import sys
from pathlib import Path

if len(sys.argv) != 2:
    sys.exit(__doc__)
SRC = Path(sys.argv[1]).expanduser().resolve()
DST = Path(__file__).resolve().parents[1] / "vendor" / "zed-cli"
C = SRC / "crates"
if not (C / "cli" / "src" / "main.rs").is_file():
    sys.exit(f"{SRC} is not a Rubiks IDE (Zed) checkout")
COMMIT = subprocess.check_output(["git", "-C", str(SRC), "rev-parse", "HEAD"], text=True).strip()


def lines(path):
    return (C / path).read_text().split("\n")


def block(path, start_pat, end_pat):
    """From the first line matching start_pat, with the doc comments and
    attributes directly above it, through the next line matching end_pat."""
    text = lines(path)
    start = next(i for i, line in enumerate(text) if re.search(start_pat, line))
    while start > 0 and re.match(r"\s*(///|#\[)", text[start - 1]):
        start -= 1
    end = next(i for i in range(start + 1, len(text)) if re.search(end_pat, text[i]))
    return "\n".join(text[start : end + 1])


def write(rel, content):
    path = DST / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    # Removing items can leave runs of blank lines; rustfmt keeps at most one.
    content = re.sub(r"\n{3,}", "\n\n", content)
    path.write_text(content.rstrip("\n") + "\n")


def header(origin):
    return (
        f"// Copied from Rubiks IDE (a Zed fork), {origin}, at commit {COMMIT[:10]}.\n"
        "// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.\n\n"
    )


# Start clean, keeping the hand-written README.
DST.mkdir(parents=True, exist_ok=True)
for child in DST.iterdir():
    if child.name != "README.md":
        shutil.rmtree(child) if child.is_dir() else child.unlink()

# ---- licences --------------------------------------------------------------
shutil.copy(SRC / "LICENSE-GPL", DST / "LICENSE-GPL")
shutil.copy(SRC / "LICENSE-APACHE", DST / "LICENSE-APACHE")

# ---- cli: unchanged sources ------------------------------------------------
(DST / "crates/cli/src").mkdir(parents=True)
for name in ["cli.rs", "main.rs", "completions.rs"]:
    shutil.copy(C / "cli/src" / name, DST / "crates/cli/src" / name)
shutil.copy(C / "cli/README.md", DST / "crates/cli/README.md")
build = (C / "cli/build.rs").read_text()
windows_block = """    #[cfg(windows)]
    {
        println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
        println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");

        windows_resources::compile(false).expect("failed to compile Windows resources");
    }
"""
assert windows_block in build, "cli/build.rs changed upstream"
write("crates/cli/build.rs", build.replace(windows_block,
    "    // Zed's Windows icon/version resources (windows_resources) are not vendored.\n"))

# The CLI's `--uninstall` embeds this script by a path relative to Zed's layout
# (crates/cli/src/../../../script), which this directory mirrors.
(DST / "script").mkdir()
shutil.copy(SRC / "script/uninstall.sh", DST / "script/uninstall.sh")

# ---- askpass ---------------------------------------------------------------
write("crates/askpass/src/askpass.rs", header("crates/askpass/src/askpass.rs")
      + block("askpass/src/askpass.rs", r"^/// Runs Zed in netcat mode", r"^}$",
              ) + "\n\n"
      + block("askpass/src/askpass.rs", r"^/// Runs Zed in askpass mode using prompts", r"^}$") + "\n\n"
      + block("askpass/src/askpass.rs", r"^fn connect_and_write_prompt", r"^}$"))

# ---- net: whole crate ------------------------------------------------------
shutil.copytree(C / "net/src", DST / "crates/net/src")

# ---- collections -----------------------------------------------------------
write("crates/collections/src/collections.rs", header("crates/collections/src/collections.rs")
      + "pub type HashMap<K, V> = FxHashMap<K, V>;\n"
        "pub type HashSet<T> = FxHashSet<T>;\n\n"
        "pub use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet, FxHasher};\n"
        "pub use std::collections::*;\n")
assert "pub type HashMap<K, V> = FxHashMap<K, V>;" in (C / "collections/src/collections.rs").read_text()

# ---- paths -----------------------------------------------------------------
p = lines("paths/src/paths.rs")
app = next(i for i, l in enumerate(p) if l.startswith("/// The application name, used to derive"))
cfg_end = next(i for i, l in enumerate(p) if l.startswith("static CONFIG_DIR: OnceLock<PathBuf>"))
consts = "\n".join(p[app : cfg_end + 1])
write("crates/paths/src/paths.rs", "//! Paths to locations used by Zed.\n" + header("crates/paths/src/paths.rs")
      + "use std::path::PathBuf;\nuse std::sync::OnceLock;\nuse util::paths::SanitizedPath;\n\npub use util::paths::home_dir;\n\n"
      + consts + "\n\n"
      + block("paths/src/paths.rs", r"^pub fn set_custom_data_dir", r"^}$") + "\n\n"
      + block("paths/src/paths.rs", r"^pub fn config_dir\(\)", r"^}$") + "\n\n"
      + block("paths/src/paths.rs", r"^pub fn data_dir\(\)", r"^}$"))

# ---- release_channel -------------------------------------------------------
r = lines("release_channel/src/lib.rs")
top_end = next(i for i, l in enumerate(r) if l.startswith("/// The app identifier for the current release channel"))
top = "\n".join(r[:top_end]).replace("use gpui::{App, Global};\nuse semver::Version;\n", "")
assert "gpui" not in top and "semver" not in top
ident = block("release_channel/src/lib.rs", r"^pub fn app_identifier", r"^}$")
enum = block("release_channel/src/lib.rs", r"^pub enum ReleaseChannel", r"^}$")
impl = block("release_channel/src/lib.rs", r"^impl ReleaseChannel \{", r"^}$")
# drop the two methods that read the running app's gpui globals
impl = re.sub(r"\n    /// Returns the global \[`ReleaseChannel`\]\.\n    pub fn global\(cx: &App\) -> Self \{\n.*?\n    \}\n", "\n", impl, flags=re.S)
impl = re.sub(r"\n    /// Returns the global \[`ReleaseChannel`\], if one is set\.\n    pub fn try_global\(cx: &App\) -> Option<Self> \{\n.*?\n    \}\n", "\n", impl, flags=re.S)
assert "cx" not in impl and "App" not in impl.replace("AppCommitSha", ""), impl[:400]
# `pub struct InvalidReleaseChannel;` is one line, so take it with its attributes
# and doc comment directly rather than through block().
inv = next(i for i, l in enumerate(r) if l.startswith("pub struct InvalidReleaseChannel;"))
inv_start = inv
while re.match(r"\s*(///|#\[)", r[inv_start - 1]):
    inv_start -= 1
fromstr = "\n".join(r[inv_start : inv + 1]) + "\n\n" + \
    block("release_channel/src/lib.rs", r"^impl FromStr for ReleaseChannel", r"^}$")
tests = block("release_channel/src/lib.rs", r"^mod tests \{", r"^}$")
write("crates/release_channel/src/lib.rs", top.replace("\n\nuse std::", "\n\n" + header("crates/release_channel/src/lib.rs").rstrip() + "\n\nuse std::", 1)
      + ident + "\n\n" + enum + "\n\n" + impl + "\n\n" + fromstr + "\n\n" + tests)
shutil.copy(C / "release_channel/build.rs", DST / "crates/release_channel/build.rs")
(DST / "crates/zed").mkdir(parents=True)
shutil.copy(C / "zed/RELEASE_CHANNEL", DST / "crates/zed/RELEASE_CHANNEL")

# ---- util ------------------------------------------------------------------
home = block("util/src/paths.rs", r"^pub fn home_dir\(\)", r"^}$")
sanitized = block("util/src/paths.rs", r"^pub struct SanitizedPath\(Path\);", r"^impl AsRef<Path> for SanitizedPath", )
sanitized = sanitized + "\n" + "\n".join(lines("util/src/paths.rs")[
    next(i for i, l in enumerate(lines("util/src/paths.rs")) if l.startswith("impl AsRef<Path> for SanitizedPath")) + 1 :
    next(i for i, l in enumerate(lines("util/src/paths.rs")) if l.startswith("impl AsRef<Path> for SanitizedPath")) + 5])
assert sanitized.rstrip().endswith("}"), sanitized[-200:]
pwp_start = next(i for i, l in enumerate(lines("util/src/paths.rs")) if l.startswith("/// A delimiter to use in `path_query:row_number"))
pwp_impl = next(i for i, l in enumerate(lines("util/src/paths.rs")) if l.startswith("impl PathWithPosition {"))
pwp_end = next(i for i in range(pwp_impl, len(lines("util/src/paths.rs"))) if lines("util/src/paths.rs")[i] == "}")
pwp = "\n".join(lines("util/src/paths.rs")[pwp_start : pwp_end + 1])
write("crates/util/src/paths.rs", header("crates/util/src/paths.rs")
      + "use regex::Regex;\nuse serde::{Deserialize, Serialize};\nuse std::fmt::{Display, Formatter};\n"
        "use std::mem;\nuse std::path::StripPrefixError;\nuse std::path::{Path, PathBuf};\n"
        "use std::sync::{Arc, LazyLock};\n\n"
      + home + "\n\n" + sanitized + "\n\n" + pwp)
g = (C / "gpui_util/src/lib.rs").read_text().split("\n")
gs = next(i for i, l in enumerate(g) if l == '#[cfg(target_os = "windows")]' and g[i + 1].startswith("const CREATE_NO_WINDOW"))
ge = next(i for i in range(gs, len(g)) if g[i].startswith("pub fn new_std_command") and g[i - 1] == '#[cfg(not(target_os = "windows"))]')
ge = next(i for i in range(ge, len(g)) if g[i] == "}")
write("crates/util/src/command.rs", header("crates/gpui_util/src/lib.rs (re-exported as util::command)")
      + "use std::ffi::OsStr;\n\n" + "\n".join(g[gs : ge + 1]))
t = lines("util/src/test.rs")
ts = next(i for i, l in enumerate(t) if l == "use std::ffi::OsStr;")
te = next(i for i, l in enumerate(t) if l.startswith("pub fn sample_text")) - 1
write("crates/util/src/test.rs", header("crates/util/src/test.rs") + "\n".join(t[ts:te]))
root = block("util/src/util.rs", r"^pub fn prevent_root_execution\(\)", r"^}$")
write("crates/util/src/util.rs", header("crates/util/src/util.rs")
      + "pub mod command;\npub mod paths;\n#[cfg(any(test, feature = \"test-support\"))]\npub mod test;\n\n"
        "pub use util_macros::path;\n\n" + root)

# ---- util_macros: the path! macro -------------------------------------------
write("crates/util_macros/src/util_macros.rs", header("crates/util_macros/src/util_macros.rs")
      + "use proc_macro::TokenStream;\nuse quote::quote;\nuse syn::{LitStr, parse_macro_input};\n\n"
      + block("util_macros/src/util_macros.rs", r"^pub fn path\(input: TokenStream\)", r"^}$"))


# ---- Cargo manifests --------------------------------------------------------
# Same dependency versions and features as the Rubiks IDE workspace.
ws = (SRC / "Cargo.toml").read_text()
m = re.search(r'\[workspace\.dependencies\.windows\]\nversion = "([^"]+)"\nfeatures = \[(.*?)\]', ws, re.S)
WINDOWS = f'windows = {{ version = "{m.group(1)}", features = [{" ".join(m.group(2).split())}] }}'


# Zed's own workspace lint settings, so the vendored code is checked the way Zed
# checks it rather than against this workspace's pedantic set. `disallowed_methods`
# is left out: it enforces a list in Zed's clippy.toml, which is not vendored.
ZED_LINTS = """
[lints.rust]
unexpected_cfgs = { level = "allow" }
{extra_rust}
[lints.clippy]
dbg_macro = "deny"
todo = "deny"
declare_interior_mutable_const = "deny"
redundant_clone = "deny"
style = { level = "allow", priority = -1 }
type_complexity = "allow"
let_underscore_future = "allow"
single_range_in_vec_init = "allow"
too_many_arguments = "allow"
large_enum_variant = "allow"
nonminimal_bool = "allow"
"""


def package(name, licence, description, extra_rust=""):
    return f"""# Generated by scripts/vendor_zed_cli.py; see ../../README.md.
[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
publish = false
license = "{licence}"
description = "{description}"
""" + ZED_LINTS.replace("{extra_rust}", extra_rust)


write("crates/cli/Cargo.toml", package("zed-cli", "GPL-3.0-or-later",
    "Zed's command-line interface, built as `rubiks-ide`: opens files, folders and diffs in Rubiks IDE.") + f"""
[lib]
name = "cli"
path = "src/cli.rs"
doctest = false

[[bin]]
name = "rubiks-ide"
path = "src/main.rs"

[features]
no-bundled-uninstall = []
default = []

[dependencies]
anyhow = "1.0.86"
askpass = {{ package = "zed-askpass", path = "../askpass" }}
clap = {{ version = "4.4", features = ["derive", "wrap_help"] }}
clap_complete = "4.4"
clap_complete_nushell = "4.4"
collections = {{ package = "zed-collections", path = "../collections" }}
console = "0.16"
dialoguer = {{ version = "0.12", default-features = false }}
ipc-channel = "0.19"
parking_lot = "0.12.1"
paths = {{ package = "zed-paths", path = "../paths" }}
release_channel = {{ package = "zed-release-channel", path = "../release_channel" }}
serde = {{ version = "1.0.221", features = ["derive", "rc"] }}
util = {{ package = "zed-util", path = "../util" }}
tempfile = "3.20.0"
rayon = "1.8"
walkdir = "2.5"

[dev-dependencies]
serde_json = {{ version = "1.0.144", features = ["preserve_order", "raw_value"] }}
util = {{ package = "zed-util", path = "../util", features = ["test-support"] }}

[target.'cfg(any(target_os = "linux", target_os = "freebsd"))'.dependencies]
exec = "0.3.1"
fork = "0.4.0"

[target.'cfg(target_os = "macos")'.dependencies]
core-foundation = "0.10"
core-services = "1"
plist = "1.3"

[target.'cfg(target_os = "windows")'.dependencies]
{WINDOWS}
""")
write("crates/askpass/Cargo.toml", package("zed-askpass", "GPL-3.0-or-later",
    "The askpass (netcat-over-socket) helper of Zed's CLI.") + """
[lib]
name = "askpass"
path = "src/askpass.rs"
doctest = false

[dependencies]
net = { package = "zed-net", path = "../net" }
""")
write("crates/net/Cargo.toml", package("zed-net", "GPL-3.0-or-later",
    "Cross-platform Unix-domain sockets, as used by Zed.") + f"""
[lib]
name = "net"
path = "src/net.rs"
doctest = false

[dependencies]
smol = "2.0"

[target.'cfg(target_os = "windows")'.dependencies]
async-io = "2.4"
{WINDOWS}

[dev-dependencies]
tempfile = "3.20.0"
nix = {{ version = "0.30", features = ["resource", "user"] }}
""")
write("crates/collections/Cargo.toml", package("zed-collections", "Apache-2.0",
    "Zed's collection type aliases (FxHash-based).") + """
[lib]
name = "collections"
path = "src/collections.rs"
doctest = false

[dependencies]
rustc-hash = "2.1.0"
""")
write("crates/paths/Cargo.toml", package("zed-paths", "GPL-3.0-or-later",
    "Zed's data and config directory locations.") + """
[lib]
name = "paths"
path = "src/paths.rs"
doctest = false

[dependencies]
dirs = "6.0"
util = { package = "zed-util", path = "../util" }
""")
write("crates/release_channel/Cargo.toml", package("zed-release-channel", "GPL-3.0-or-later",
    "Zed's release channel (stable, preview, nightly, dev).") + """
[lib]
name = "release_channel"
path = "src/lib.rs"
doctest = false
""")
write("crates/util/Cargo.toml", package("zed-util", "Apache-2.0",
    "The parts of Zed's util crate that Zed's CLI uses.") + """
[lib]
name = "util"
path = "src/util.rs"
doctest = false

[features]
test-support = ["dep:tempfile", "dep:serde_json"]

[dependencies]
dirs = "6.0"
regex = "1.5"
serde = { version = "1.0.221", features = ["derive", "rc"] }
serde_json = { version = "1.0.144", features = ["preserve_order", "raw_value"], optional = true }
tempfile = { version = "3.20.0", optional = true }
util_macros = { package = "zed-util-macros", path = "../util_macros" }

[target.'cfg(unix)'.dependencies]
nix = { version = "0.30", features = ["user"] }

[target.'cfg(target_os = "windows")'.dependencies]
dunce = "1.0"
""")
write("crates/util_macros/Cargo.toml", package("zed-util-macros", "Apache-2.0",
    "The path! macro from Zed's util_macros crate.",
    "# `path` is only reassigned on Windows; the source is kept as Zed wrote it.\nunused_mut = \"allow\"\n") + """
[lib]
name = "util_macros"
path = "src/util_macros.rs"
proc-macro = true
doctest = false

[dependencies]
quote = "1.0.9"
syn = { version = "2.0.101", features = ["full", "extra-traits", "visit-mut"] }

""")

print(f"Vendored Zed's CLI from {SRC} at {COMMIT[:10]} into {DST}")
