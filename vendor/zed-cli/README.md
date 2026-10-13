# Zed CLI (vendored)

Zed's command-line interface, built here as **`rubiks-ide`**. It opens files, folders and diffs
in Rubiks IDE (a Zed fork) from a terminal, the same program Zed installs as `zed`.

Copied from [rubiksworkspace/Rubiks-IDE](https://github.com/rubiksworkspace/Rubiks-IDE) at
commit `908f62b01c` by `scripts/vendor_zed_cli.py`.

## Build and use

```bash
cargo build -p zed-cli            # target/debug/rubiks-ide
cargo build --release -p zed-cli  # target/release/rubiks-ide
```

On macOS the CLI finds the editor from the app bundle it ships inside. Run from here, tell it
where Rubiks IDE is with `--zed`, either the `.app` or the editor binary:

```bash
rubiks-ide --zed "/Applications/Rubiks IDE.app" src/main.rs:42:7
rubiks-ide --zed ~/Rubiks-IDE/target/debug/zed --version
```

Every option of Zed's CLI is available: open paths (with `path:line:column`), `--wait`,
`--add`, `--new`, `--existing`, `--diff`, `--user-data-dir`, `--foreground`,
`--dev-container`, `--dev-server-token`, `--version`, `--system-specs`, `--completions <SHELL>`
and `--uninstall`, shown by `rubiks-ide --help`. Also present but not listed there: `--wsl`
(compiled into Windows builds only), and the hidden `--reuse`, `--classic` and `--askpass`
options, which other programs call; `--askpass` (or `ZED_ASKPASS_SOCKET`) runs the helper that
passes SSH and git password prompts to the editor.

## What was copied, and what changed

The layout mirrors the Zed repository (`crates/`, `script/`) so the CLI's relative paths keep
working.

| Path | Origin | Changes |
|---|---|---|
| `crates/cli/src/*.rs`, `crates/cli/README.md` | `crates/cli` | None (byte-identical) |
| `crates/cli/build.rs` | `crates/cli/build.rs` | Drops the Windows icon/version resource step (`windows_resources` is not vendored) |
| `crates/net/src` | `crates/net` | None (whole crate) |
| `crates/askpass` | `crates/askpass` | Only `main`, `main_from_args` and the socket helper they share |
| `crates/collections` | `crates/collections` | Only the `HashMap`/`HashSet` aliases and re-exports |
| `crates/paths` | `crates/paths` | Only the app name, data and config directory functions |
| `crates/release_channel` | `crates/release_channel` | Without the app version types and the two methods that read GPUI globals |
| `crates/util` | `crates/util`, `crates/gpui_util` | Only `home_dir`, `SanitizedPath`, `PathWithPosition`, `new_std_command`, `prevent_root_execution` and the test `TempTree` |
| `crates/util_macros` | `crates/util_macros` | Only the `path!` macro |
| `crates/zed/RELEASE_CHANNEL`, `script/uninstall.sh` | same paths | None |

Every copied item is unchanged. The support crates keep their original crate names (`askpass`,
`util`, and so on, through package renames in each `Cargo.toml`), so the CLI's `use` paths resolve
as written.

## Licences

| Crate | Licence |
|---|---|
| `cli`, `askpass`, `net`, `paths`, `release_channel` | GPL-3.0-or-later ([LICENSE-GPL](LICENSE-GPL)) |
| `collections`, `util`, `util_macros` | Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)) |

Because this code is part of the workspace, the harness on this branch is released under
GPL-3.0-or-later as a whole; see the repository README.

## Updating

```bash
python3 scripts/vendor_zed_cli.py <path to a Rubiks-IDE checkout>
```

It replaces everything here except this README and stops with an error if an upstream change no
longer matches what it copies. Update the commit above after running it.

## Testing

`cargo test -p zed-cli` runs the CLI's own tests. One of them (`test_restart_cli_args`) lives in
the Flatpak module and only builds on Linux. The Windows code paths are copied unchanged but have
not been built here.
