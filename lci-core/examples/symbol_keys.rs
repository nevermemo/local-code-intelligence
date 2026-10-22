//! Counts how often the Rust symbol key (`node-id.md`, lesson L1) collides
//! over one commit of a repository: every tracked `.rs` blob at `--rev` goes
//! through `rust_declarations`, and the joined Symbol NodeIds are compared
//! across the whole tree. The symbol-key half of the slice S1 evidence
//! (`docs/spike/multimap/s1-stability.md`). Reads git objects only.
//!
//! ```text
//! cargo run -p lci-core --example symbol_keys -- [--repo DIR] [--rev REV]
//! ```
//!
//! Inside one file the ids are distinct by construction (the disambiguator
//! counts repeats of the rest of the key). What is measured is how often
//! that disambiguator fires and whether two files join to the same id (the
//! module path comes from the file path, so that is a property of the tree,
//! not of the extractor).

use anyhow::{Context, Result, bail, ensure};
use lci_core::node::{NodeId, rust_declarations};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Command,
};

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn main() -> Result<()> {
    let (mut repo, mut rev) = (PathBuf::from("."), "HEAD".to_owned());
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        let value = raw
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--repo" => repo = value.into(),
            "--rev" => rev = value,
            _ => bail!("unknown flag {flag}"),
        }
    }
    let tree = String::from_utf8(git(&repo, &["ls-tree", "-r", "-z", &rev])?)?;
    let mut files = 0;
    let mut failures = Vec::new();
    let mut declarations = 0;
    let mut disambiguated = Vec::new();
    let mut kinds = BTreeMap::<&str, usize>::new();
    let mut by_id = HashMap::<String, Vec<String>>::new();
    for entry in tree.split('\0').filter(|e| !e.is_empty()) {
        let (meta, path) = entry.split_once('\t').context("ls-tree entry")?;
        let fields: Vec<&str> = meta.split(' ').collect();
        if fields[1] != "blob"
            || fields[0] == "120000"
            || !lci_core::path::has_extension_ignore_case(path, "rs")
        {
            continue;
        }
        files += 1;
        let bytes = git(&repo, &["cat-file", "blob", fields[2]])?;
        let source = String::from_utf8_lossy(&bytes);
        let parsed = match rust_declarations(&source) {
            Ok(parsed) => parsed,
            Err(error) => {
                failures.push(format!("{path}: {error}"));
                continue;
            }
        };
        for declaration in parsed {
            declarations += 1;
            *kinds.entry(declaration.kind.as_str()).or_default() += 1;
            let id = declaration.node_id(path);
            debug_assert!(matches!(id, NodeId::Symbol(_)));
            if declaration.disambiguator > 0 {
                disambiguated.push(format!("{id} ({path}:{})", declaration.start_line));
            }
            by_id
                .entry(id.to_string())
                .or_default()
                .push(format!("{path}:{}", declaration.start_line));
        }
    }
    let shared: Vec<_> = by_id.iter().filter(|(_, at)| at.len() > 1).collect();
    println!("rev {rev}: {files} tracked .rs files");
    println!("parse failures: {}", failures.len());
    for failure in &failures {
        println!("  {failure}");
    }
    println!("declarations: {declarations}");
    println!("distinct symbol ids: {}", by_id.len());
    println!(
        "declarations with disambiguator > 0: {}",
        disambiguated.len()
    );
    for id in &disambiguated {
        println!("  {id}");
    }
    println!(
        "symbol ids produced by two or more declarations: {}",
        shared.len()
    );
    for (id, at) in shared {
        println!("  {id}: {}", at.join(", "));
    }
    let kinds: Vec<String> = kinds
        .iter()
        .map(|(kind, count)| format!("{kind} {count}"))
        .collect();
    println!("kinds: {}", kinds.join(", "));
    Ok(())
}
