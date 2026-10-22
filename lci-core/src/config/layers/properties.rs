//! N59 (e): the local overlay round trip and layer precedence. A seeded loop
//! builds random overlays (strings with quotes, backslashes, control and
//! non-ASCII characters; notes that try to smuggle in settings) and checks
//! that `LocalOverrides::parse(render_overlay(..))` gives the overlay back. A
//! second loop sets each machine-set key in a random subset of the layers and
//! checks that `resolve_unvalidated` takes the value, and reports the source,
//! of the highest layer that sets it: file, then overlay, then environment,
//! then flag.

use super::{
    Flags, LSP_SECTIONS, LocalOverrides, LocalState, LspOverride, OverlayNote, ResolveSpec,
    SCALAR_KEYS, Source, env_var_for_key, machine_keys, render_overlay, resolve_unvalidated,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};

/// SplitMix64: a small deterministic generator, so a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self) -> bool {
        self.below(2) == 0
    }
}

/// Pieces of strings: TOML quoting characters, Windows paths, control
/// characters (including DEL and a C1 control), and non-ASCII text.
const PIECES: &[&str] = &[
    "a",
    "C:\\Program Files\\x",
    "/usr/bin/x",
    "'",
    "''",
    "'''",
    "\"",
    "\\",
    "\\n",
    "\n",
    "\r\n",
    "\t",
    "\u{0}",
    "\u{1b}",
    "\u{7f}",
    "\u{85}",
    "é",
    "中",
    "🦀",
    " ",
    "#",
    "=",
    "[go]",
    "\u{2028}",
    "${HOME}",
];

fn string(rng: &mut Rng) -> String {
    (0..rng.below(4))
        .map(|_| PIECES[rng.below(PIECES.len())])
        .collect()
}

fn lsp(rng: &mut Rng) -> Option<LspOverride> {
    rng.chance().then(|| LspOverride {
        path: rng.chance().then(|| string(rng)),
        args: rng
            .chance()
            .then(|| (0..rng.below(3)).map(|_| string(rng)).collect()),
        disabled: rng.chance().then(|| rng.chance()),
    })
}

fn overrides(rng: &mut Rng) -> LocalOverrides {
    LocalOverrides {
        data_dir: rng.chance().then(|| PathBuf::from(string(rng))),
        port: rng.chance().then(|| rng.next() as u16),
        embedding_url: rng.chance().then(|| string(rng)),
        embedding_model: rng.chance().then(|| string(rng)),
        reranker_url: rng.chance().then(|| string(rng)),
        reranker_model: rng.chance().then(|| string(rng)),
        ripgrep_path: rng.chance().then(|| string(rng)),
        rust_analyzer_path: rng.chance().then(|| string(rng)),
        csharp: lsp(rng),
        typescript: lsp(rng),
        python: lsp(rng),
        go: lsp(rng),
        java: lsp(rng),
        clangd: lsp(rng),
        markdown: lsp(rng),
    }
}

/// A language-server section with no field set writes nothing and reads back
/// as absent; both change nothing when applied. No producer builds one today
/// (`lci init` sets a field whenever it creates a section, and its
/// `check_generated` in `src/init.rs` compares without this step).
fn without_empty_sections(mut overrides: LocalOverrides) -> LocalOverrides {
    for (section, _) in LSP_SECTIONS {
        let slot = overrides.lsp_mut(section);
        if slot.as_ref() == Some(&LspOverride::default()) {
            *slot = None;
        }
    }
    overrides
}

#[test]
fn rendered_overlays_parse_back_to_the_same_overrides() {
    const SEEDS: u64 = 3000;
    const NOTE_KEYS: &[&str] = &[
        "",
        "port",
        "typescript",
        "go.path",
        "unknown",
        "csharp.args",
    ];
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        let overrides = overrides(&mut rng);
        let notes: Vec<OverlayNote> = (0..rng.below(4))
            .map(|_| {
                let key = NOTE_KEYS[rng.below(NOTE_KEYS.len())];
                // A note that would set a key if it escaped its comment.
                let text = format!(
                    "{}\nport = 1\n[java]\npath = 'x'{}",
                    string(&mut rng),
                    string(&mut rng)
                );
                OverlayNote::new(key, text)
            })
            .collect();
        let header = string(&mut rng);
        let text = render_overlay(&header, &overrides, &notes);
        let parsed = LocalOverrides::parse(&text)
            .unwrap_or_else(|error| panic!("seed {seed}: {error:#}\n{text}"));
        assert_eq!(
            parsed,
            without_empty_sections(overrides),
            "seed {seed}\n{text}"
        );
    }
}

/// The layers, lowest first.
const LAYERS: [Source; 4] = [Source::File, Source::Local, Source::Env, Source::Flag];

/// The value `layer` gives `key`, as `Resolved::entries` reports it.
fn value(layer: Source, key: &str, flag: bool) -> Value {
    let rank = LAYERS.iter().position(|l| *l == layer).unwrap();
    match key.rsplit('.').next().unwrap() {
        "port" => json!(1001 + rank),
        // Two values only: the layers alternate, so neighbours differ.
        "disabled" => json!(flag),
        "args" => json!([format!("{}-arg", layer.as_str())]),
        _ => json!(format!("{}-{key}", layer.as_str())),
    }
}

/// TOML text for a value made by [`value`].
fn toml_value(value: &Value) -> String {
    match value {
        Value::String(text) => format!("'{text}'"),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(toml_value).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

/// A configuration file (or overlay) setting `values`: top-level keys, then a
/// table per language-server section.
fn toml_text(values: &BTreeMap<String, Value>) -> String {
    let mut text = String::new();
    for (key, _) in SCALAR_KEYS {
        if let Some(value) = values.get(key) {
            text.push_str(&format!("{key} = {}\n", toml_value(value)));
        }
    }
    for (section, _) in LSP_SECTIONS {
        let fields: Vec<_> = values
            .iter()
            .filter_map(|(key, value)| {
                let field = key.strip_prefix(section)?.strip_prefix('.')?;
                Some(format!("{field} = {}\n", toml_value(value)))
            })
            .collect();
        if !fields.is_empty() {
            text.push_str(&format!("[{section}]\n{}", fields.concat()));
        }
    }
    text
}

#[test]
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "`.args` ends a configuration key, not a file name"
)]
fn each_key_takes_the_highest_layer_that_sets_it() {
    const SEEDS: u64 = 200;
    let defaults: BTreeMap<String, Value> = {
        let spec = ResolveSpec {
            config_path: None,
            env: BTreeMap::new(),
            user_config_dir: None,
            flags: Flags::default(),
        };
        resolve_unvalidated(&spec)
            .unwrap()
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect()
    };
    let mut won = [0; 4];
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        let no_local_config = rng.below(5) == 0;
        let overlay_present = rng.below(4) != 0;
        let mut layers: [BTreeMap<String, Value>; 4] = Default::default();
        let mut expected = BTreeMap::new();
        for key in machine_keys() {
            // Every key but a section's `args` has a variable, so the
            // environment layer is not left out of a key by mistake.
            assert_eq!(
                env_var_for_key(&key).is_some(),
                !key.ends_with(".args"),
                "{key}"
            );
            let mut flag = rng.chance();
            let mut winner = None;
            for (rank, layer) in LAYERS.into_iter().enumerate() {
                let possible = match layer {
                    Source::Env => env_var_for_key(&key).is_some(),
                    Source::Flag => key == "port",
                    _ => true,
                };
                flag = !flag;
                if possible && rng.chance() {
                    layers[rank].insert(key.clone(), value(layer, &key, flag));
                    if layer != Source::Local || (overlay_present && !no_local_config) {
                        winner = Some((layer, value(layer, &key, flag)));
                    }
                }
            }
            expected.insert(key, winner);
        }

        std::fs::write(&config_path, toml_text(&layers[0])).unwrap();
        // The overlay beside the file, written by `render_overlay`.
        if overlay_present {
            let overlay = LocalOverrides::parse(&toml_text(&layers[1])).unwrap();
            std::fs::write(
                temp.path().join("config.local.toml"),
                render_overlay("overlay", &overlay, &[]),
            )
            .unwrap();
        }
        let env: BTreeMap<String, String> = layers[2]
            .iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                (env_var_for_key(key).unwrap(), text)
            })
            .collect();
        let spec = ResolveSpec {
            config_path: Some(config_path.clone()),
            env,
            user_config_dir: None,
            flags: Flags {
                port: layers[3]
                    .get("port")
                    .map(|port| port.as_u64().unwrap() as u16),
                no_local_config,
            },
        };
        let resolved =
            resolve_unvalidated(&spec).unwrap_or_else(|error| panic!("seed {seed}: {error:#}"));
        let state = match (no_local_config, overlay_present) {
            (true, _) => LocalState::Disabled,
            (false, true) => LocalState::Present,
            (false, false) => LocalState::Absent,
        };
        assert_eq!(resolved.layers.local.state, state, "seed {seed}");
        let entries: BTreeMap<String, (Value, Source)> = resolved
            .entries()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.machine)
            .map(|entry| (entry.key, (entry.value, entry.provenance.source)))
            .collect();
        for (key, winner) in &expected {
            let actual = &entries[key];
            let (value, source) = match winner.clone() {
                Some((layer, value)) => (value, layer),
                None => (defaults[key].clone(), Source::Default),
            };
            assert_eq!(actual, &(value, source), "seed {seed}: {key}");
            won[LAYERS.iter().position(|l| *l == source).unwrap_or(0)] +=
                usize::from(source != Source::Default);
        }
    }
    assert!(won.iter().all(|count| *count > 0), "wins per layer {won:?}");
}
