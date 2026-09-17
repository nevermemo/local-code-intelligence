//! Ported from `scripts/Build.ps1`.

use crate::acceptance::workspace_root;
use anyhow::{Result, bail};
use std::process::Command;

fn local_protoc() -> std::path::PathBuf {
    workspace_root()
        .join(".tools/protoc/bin")
        .join(format!("protoc{}", std::env::consts::EXE_SUFFIX))
}

fn run(mut command: Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        bail!("command failed: {status}");
    }
    Ok(())
}

pub fn run_build(test: bool) -> Result<()> {
    let protoc = local_protoc();
    let mut build = Command::new("cargo");
    build.args(["build", "--locked", "-j", "8"]);
    if protoc.is_file() {
        build.env("PROTOC", &protoc);
    }
    run(build)?;

    if test {
        let mut test_cmd = Command::new("cargo");
        test_cmd.args(["test", "--locked", "-j", "8"]);
        if protoc.is_file() {
            test_cmd.env("PROTOC", &protoc);
        }
        run(test_cmd)?;
    }
    Ok(())
}
