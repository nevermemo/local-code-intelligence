//! Ported from `scripts/Setup-BuildTools.ps1`: provisions `protoc` for the
//! main crate's build. Windows keeps the original auto-download-and-verify
//! behavior (a known-good SHA-256 for the official release archive).
//! macOS/Linux check PATH first and print an install hint on a miss, rather
//! than shipping download URLs/checksums for archives this Windows sandbox
//! cannot itself download and verify.

use crate::acceptance::{which, workspace_root};
use anyhow::{Context, Result, bail, ensure};
use std::io::Write;
use std::path::PathBuf;

const PROTOC_VERSION: &str = "33.0";
const WIN64_URL: &str =
    "https://github.com/protocolbuffers/protobuf/releases/download/v33.0/protoc-33.0-win64.zip";
const WIN64_SHA256: &str = "3742CD49C8B6BD78B6760540367EB0FF62FA70A1032E15DAFE131BFAF296986A";

fn tool_root() -> PathBuf {
    workspace_root().join(".tools")
}

fn protoc_path() -> PathBuf {
    tool_root()
        .join("protoc/bin")
        .join(format!("protoc{}", std::env::consts::EXE_SUFFIX))
}

pub async fn run() -> Result<()> {
    if cfg!(windows) {
        setup_windows().await?;
    } else {
        setup_unix_like()?;
    }
    verify()
}

async fn setup_windows() -> Result<()> {
    let protoc = protoc_path();
    if protoc.is_file() {
        println!("protoc already present at {}", protoc.display());
        return Ok(());
    }
    let root = tool_root();
    std::fs::create_dir_all(&root)?;
    println!("Downloading protoc {PROTOC_VERSION} for Windows...");
    let bytes = reqwest::get(WIN64_URL)
        .await
        .context("download protoc archive")?
        .bytes()
        .await
        .context("read protoc archive body")?;

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&bytes);
    let actual = digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    ensure!(
        actual == WIN64_SHA256,
        "protobuf archive checksum mismatch: expected {WIN64_SHA256}, got {actual}"
    );

    let archive_path = root.join("protoc.zip");
    std::fs::write(&archive_path, &bytes)?;
    let extract_to = root.join("protoc");
    std::fs::create_dir_all(&extract_to)?;
    let file = std::fs::File::open(&archive_path)?;
    let mut archive = zip::ZipArchive::new(file).context("open protoc archive")?;
    archive
        .extract(&extract_to)
        .context("extract protoc archive")?;
    println!("Installed protoc to {}", extract_to.display());
    Ok(())
}

fn setup_unix_like() -> Result<()> {
    if which("protoc").is_some() {
        println!("protoc found on PATH.");
    } else {
        println!(
            "protoc not found on PATH. Install it, e.g.:\n  \
             macOS:  brew install protobuf\n  \
             Debian/Ubuntu: sudo apt install protobuf-compiler\n  \
             Fedora: sudo dnf install protobuf-compiler"
        );
    }
    if which("cmake").is_none() {
        println!(
            "cmake not found on PATH. Install it, e.g.:\n  \
             macOS:  brew install cmake\n  \
             Debian/Ubuntu: sudo apt install cmake\n  \
             Fedora: sudo dnf install cmake"
        );
    }
    Ok(())
}

fn verify() -> Result<()> {
    let protoc = if protoc_path().is_file() {
        protoc_path()
    } else if let Some(found) = which("protoc") {
        found
    } else {
        bail!("protobuf compiler verification failed: no protoc available");
    };
    let output = std::process::Command::new(&protoc)
        .arg("--version")
        .output()
        .context("run protoc --version")?;
    if !output.status.success() {
        bail!("protobuf compiler verification failed");
    }
    std::io::stdout().write_all(&output.stdout)?;
    Ok(())
}
