//! Ported from `scripts/Build.ps1`.

use crate::acceptance::workspace_root;
use anyhow::{Result, bail};
use std::process::Command;

fn local_protoc() -> std::path::PathBuf {
    workspace_root()
        .join(".tools/protoc/bin")
        .join(format!("protoc{}", std::env::consts::EXE_SUFFIX))
}

/// Points `PROTOC` at the protoc that `cargo xtask setup` provisioned under
/// `.tools`, when there is one. Every cargo command that compiles the
/// workspace needs it, so `cargo xtask test` uses this too.
pub fn use_local_protoc(command: &mut Command) {
    let protoc = local_protoc();
    if protoc.is_file() {
        command.env("PROTOC", protoc);
    }
}

/// Variables `cargo run` sets for the binary it runs, which `cargo xtask`
/// (`cargo run -p xtask`) is. A nested cargo would inherit them, and build
/// scripts that track them (`ring`'s tracks `CARGO_MANIFEST_DIR`) would then
/// see a different value than in a direct `cargo` run from a shell and
/// re-run, cascading into rustls, reqwest and lance*.
const CARGO_RUN_VARS: [&str; 7] = [
    "CARGO",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_CRATE_NAME",
    "CARGO_BIN_NAME",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_TARGET_TMPDIR",
];

/// The names [`cargo_command`] removes, given the parent's variable names:
/// the fixed [`CARGO_RUN_VARS`] plus every `CARGO_PKG_*` that is present.
/// User settings such as `CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS`,
/// `CARGO_HOME`, `RUSTUP_*` and `PATH` are kept.
/// The prefix match is case-sensitive: cargo only sets uppercase names, and on
/// Windows `Command` matches the removed names case-insensitively anyway.
fn cargo_run_vars_to_remove(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut remove: Vec<String> = CARGO_RUN_VARS.iter().map(|s| s.to_string()).collect();
    remove.extend(names.into_iter().filter(|n| n.starts_with("CARGO_PKG_")));
    remove
}

/// `Command::new("cargo")` without the variables `cargo run` injected into
/// this process, so a nested cargo sees the same environment as a direct
/// one from a shell and both share one set of build artefacts. Every place
/// xtask spawns cargo goes through this.
pub fn cargo_command() -> Command {
    let mut command = Command::new("cargo");
    let names = std::env::vars_os().filter_map(|(name, _)| name.into_string().ok());
    for name in cargo_run_vars_to_remove(names) {
        command.env_remove(name);
    }
    command
}

fn run(mut command: Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        bail!("command failed: {status}");
    }
    Ok(())
}

pub fn run_build(test: bool) -> Result<()> {
    let mut build = cargo_command();
    build.args(["build", "--locked", "-j", "8"]);
    use_local_protoc(&mut build);
    run(build)?;

    if test {
        let mut test_cmd = cargo_command();
        test_cmd.args(["test", "--locked", "-j", "8"]);
        use_local_protoc(&mut test_cmd);
        run(test_cmd)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn removed(command: &Command, name: &str) -> bool {
        command
            .get_envs()
            .any(|(key, value)| key == OsStr::new(name) && value.is_none())
    }

    #[test]
    fn cargo_run_vars_to_remove_takes_pkg_vars_and_keeps_user_settings() {
        let parent = [
            "CARGO_MANIFEST_DIR",
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION_MAJOR",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_JOBS",
            "CARGO_HOME",
            "RUSTUP_TOOLCHAIN",
            "PATH",
        ]
        .map(String::from);
        let remove = cargo_run_vars_to_remove(parent);
        for name in [
            "CARGO",
            "CARGO_MANIFEST_DIR",
            "CARGO_MANIFEST_PATH",
            "CARGO_CRATE_NAME",
            "CARGO_BIN_NAME",
            "CARGO_PRIMARY_PACKAGE",
            "CARGO_TARGET_TMPDIR",
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION_MAJOR",
        ] {
            assert!(remove.iter().any(|n| n == name), "{name} kept");
        }
        for name in [
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_JOBS",
            "CARGO_HOME",
            "RUSTUP_TOOLCHAIN",
            "PATH",
        ] {
            assert!(!remove.iter().any(|n| n == name), "{name} removed");
        }
    }

    /// `cargo test` sets `CARGO_MANIFEST_DIR` and `CARGO_PKG_NAME` for this
    /// test binary, the same way `cargo run` does for `cargo xtask`, so the
    /// parent environment already holds them and nothing here mutates it.
    #[test]
    fn cargo_command_removes_inherited_cargo_run_vars() {
        assert!(
            std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
            "run under cargo test"
        );
        let command = cargo_command();
        assert_eq!(command.get_program(), OsStr::new("cargo"));
        assert!(removed(&command, "CARGO_MANIFEST_DIR"));
        if std::env::var_os("CARGO_PKG_NAME").is_some() {
            assert!(removed(&command, "CARGO_PKG_NAME"));
        }
        assert!(!command.get_envs().any(|(key, _)| key == OsStr::new("PATH")));
    }
}
