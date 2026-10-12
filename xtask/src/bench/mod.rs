//! `cargo xtask bench ...`: the multimap spike bench
//! (`docs/spike/multimap/bench.md`).

pub mod mine;

use anyhow::{Result, bail};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum BenchCommand {
    /// Mine replayed-commit tasks (a bench set) from local clones of public
    /// repositories. Reads git history only; writes data, never code.
    Mine {
        /// `<key>=<path to a clone>`; repeat per repository. The URL is read
        /// from the clone's `origin` remote and the snapshot is its HEAD.
        #[arg(long = "source", required = true)]
        sources: Vec<String>,
        /// `<key>=<SPDX licence id>` for every source, checked by hand.
        #[arg(long = "license", required = true)]
        licenses: Vec<String>,
        /// Keep at most this many tasks per repository (the most recent).
        #[arg(long, default_value_t = 20)]
        max: usize,
        /// Version label written into the set.
        #[arg(long, default_value = "v1")]
        version: String,
        /// Bench set file to write.
        #[arg(long)]
        out: PathBuf,
    },
}

fn split_pair(text: &str, what: &str) -> Result<(String, String)> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() && !value.is_empty() => {
            Ok((key.to_owned(), value.to_owned()))
        }
        _ => bail!("{what} must be <key>=<value>, got {text:?}"),
    }
}

pub fn run(command: BenchCommand) -> Result<()> {
    match command {
        BenchCommand::Mine {
            sources,
            licenses,
            max,
            version,
            out,
        } => {
            let licenses = licenses
                .iter()
                .map(|text| split_pair(text, "--license"))
                .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
            let sources = sources
                .iter()
                .map(|text| {
                    let (key, path) = split_pair(text, "--source")?;
                    let Some(license) = licenses.get(&key) else {
                        bail!("no --license given for source {key}");
                    };
                    Ok(mine::Source {
                        license: license.clone(),
                        key,
                        path: PathBuf::from(path),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            mine::run(sources, max, &version, &out)
        }
    }
}
