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

fn run(mut command: Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        bail!("command failed: {status}");
    }
    Ok(())
}

pub fn run_build(test: bool) -> Result<()> {
    let mut build = Command::new("cargo");
    build.args(["build", "--locked", "-j", "8"]);
    use_local_protoc(&mut build);
    run(build)?;

    if test {
        let mut test_cmd = Command::new("cargo");
        test_cmd.args(["test", "--locked", "-j", "8"]);
        use_local_protoc(&mut test_cmd);
        run(test_cmd)?;
    }
    Ok(())
}
