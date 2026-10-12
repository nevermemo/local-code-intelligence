//! N59 (c): metamorphic tests of path normalisation and path filters. A seeded
//! loop builds random repository paths and glob patterns and checks relations
//! that must hold whatever the inputs: `normalize_path` is idempotent and
//! blind to how separators and `.` components are spelled, a `GlobPattern`
//! answers the same for every spelling of a path and of itself, and
//! `EffectiveFilter::matches` is the conjunction of its parts and does not
//! depend on the order or repetition of its patterns.

use super::source_role::normalize_path;
use super::{EffectiveFilter, FilterError, FilterRequest, GlobPattern, SourceRole};

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

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

/// Path components, including ones that look like globs or drives and
/// non-ASCII ones.
const COMPONENTS: &[&str] = &[
    "src", "lib", "tests", "Src", "a.rs", "b.py", "main.go", "x y.ts", "é.rs", "a:b", ".hidden",
    "..x", "v1.2", "[x]", "*",
];

/// Glob components; `**` crosses directories, the others do not.
const GLOB_COMPONENTS: &[&str] = &[
    "src", "lib", "tests", "a.rs", "*", "**", "*.rs", "?.rs", "*.py", "a?b", "é.rs", "x y.ts",
];

fn path(rng: &mut Rng) -> String {
    (0..rng.below(4) + 1)
        .map(|_| *rng.pick(COMPONENTS))
        .collect::<Vec<_>>()
        .join("/")
}

fn pattern(rng: &mut Rng) -> String {
    (0..rng.below(3) + 1)
        .map(|_| *rng.pick(GLOB_COMPONENTS))
        .collect::<Vec<_>>()
        .join("/")
}

/// Another spelling of the same repository-relative path: any separator may be
/// a backslash or doubled, `.` components may appear anywhere, and a leading
/// or trailing separator may be added.
/// The leading separator pins current behaviour: `normalize_path` treats a
/// rooted path (`/src/a.rs`) as the relative one (`src/a.rs`).
fn respell(rng: &mut Rng, path: &str) -> String {
    let mut out = String::new();
    if rng.below(3) == 0 {
        out.push_str(rng.pick::<&str>(&["./", "/", "\\", ".\\"]));
    }
    for (index, component) in path.split('/').enumerate() {
        if index > 0 {
            out.push_str(rng.pick::<&str>(&["/", "\\", "//", "/./", "\\.\\"]));
        }
        out.push_str(component);
    }
    if rng.below(3) == 0 {
        out.push_str(rng.pick::<&str>(&["/", "\\", "/."]));
    }
    out
}

/// Another spelling of the same pattern: surrounding whitespace, a leading
/// `./`, inner `.` components and, with `doubled`, doubled separators.
fn respell_pattern(rng: &mut Rng, pattern: &str, doubled: bool) -> String {
    let separators: &[&str] = if doubled {
        &["/", "/./", "//"]
    } else {
        &["/", "/./"]
    };
    let mut out = String::from(*rng.pick(&["", " ", "\t"]));
    if rng.below(2) == 0 {
        out.push_str("./");
    }
    for (index, component) in pattern.split('/').enumerate() {
        if index > 0 {
            out.push_str(rng.pick::<&str>(separators));
        }
        out.push_str(component);
    }
    out.push_str(rng.pick::<&str>(&["", " ", "\n"]));
    out
}

// Glob compilation dominates the run time (a debug build compiles every
// pattern), so the loops that compile patterns run fewer seeds.
const SEEDS: u64 = 4000;
const GLOB_SEEDS: u64 = 1500;
const FILTER_SEEDS: u64 = 500;

#[test]
fn normalize_path_is_idempotent_and_ignores_the_spelling_of_separators() {
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        let path = path(&mut rng);
        let normal = normalize_path(&path);
        // Generated paths are already normal.
        assert_eq!(normal, path, "seed {seed}");
        for _ in 0..4 {
            let spelled = respell(&mut rng, &path);
            let normalized = normalize_path(&spelled);
            assert_eq!(normalized, normal, "seed {seed}: {spelled:?}");
            assert_eq!(normalize_path(&normalized), normalized, "seed {seed}");
            assert!(
                !normalized.contains('\\')
                    && !normalized.starts_with('/')
                    && !normalized.ends_with('/')
                    && normalized.split('/').all(|c| !c.is_empty() && c != "."),
                "seed {seed}: {normalized:?}"
            );
        }
        // Concatenation commutes with normalisation.
        let other = self::path(&mut rng);
        assert_eq!(
            normalize_path(&format!(
                "{}/{}",
                respell(&mut rng, &path),
                respell(&mut rng, &other)
            )),
            format!("{normal}/{other}"),
            "seed {seed}"
        );
    }
}

#[test]
fn glob_patterns_answer_the_same_for_every_spelling() {
    glob_patterns_answer_the_same(false);
}

/// Finding C1: doubled separators in a glob pattern should be equivalent to
/// single separators, as they are in normalized paths.
#[test]
fn glob_patterns_with_doubled_separators_answer_the_same() {
    let single = GlobPattern::compile("src/*.rs").unwrap();
    let doubled = GlobPattern::compile("src//*.rs").unwrap();
    assert!(single.matches("src/a.rs"));
    assert_eq!(doubled.matches("src/a.rs"), single.matches("src/a.rs"));
    glob_patterns_answer_the_same(true);
}

fn glob_patterns_answer_the_same(doubled: bool) {
    let mut compiled = 0;
    let mut matched = 0;
    for seed in 0..GLOB_SEEDS {
        let mut rng = Rng(seed);
        let raw = pattern(&mut rng);
        let glob = match GlobPattern::compile(&raw) {
            Ok(glob) => glob,
            Err(error) => panic!("seed {seed}: {raw:?} does not compile: {error}"),
        };
        compiled += 1;
        let variant = respell_pattern(&mut rng, &raw, doubled);
        let respelled = GlobPattern::compile(&variant)
            .unwrap_or_else(|error| panic!("seed {seed}: {variant:?}: {error}"));
        let anywhere = GlobPattern::compile(&format!("**/{raw}")).unwrap();
        for _ in 0..8 {
            let path = path(&mut rng);
            let expected = glob.matches(&path);
            matched += usize::from(expected);
            let spelled = respell(&mut rng, &path);
            let context = format!("seed {seed}: {raw:?} / {variant:?} on {path:?} / {spelled:?}");
            assert_eq!(glob.matches(&spelled), expected, "{context}");
            assert_eq!(respelled.matches(&path), expected, "{context}");
            assert_eq!(respelled.matches(&spelled), expected, "{context}");
            // `**/` adds zero or more leading directories.
            if expected {
                assert!(anywhere.matches(&path), "{context}");
                assert!(anywhere.matches(&format!("src/{path}")), "{context}");
            }
        }
        // A pattern without wildcards matches exactly its own path.
        if !raw.contains(['*', '?']) {
            assert!(glob.matches(&raw), "seed {seed}: {raw:?}");
            assert!(!glob.matches(&format!("{raw}x")), "seed {seed}: {raw:?}");
            assert!(!glob.matches(&format!("x/{raw}")), "seed {seed}: {raw:?}");
        }
        // Rooted, drive-prefixed and traversing spellings are refused.
        for (bad, absolute) in [
            (format!("/{raw}"), true),
            (format!("//{raw}"), true),
            (format!("C:/{raw}"), true),
            (format!("../{raw}"), false),
            (format!("{raw}/../x"), false),
        ] {
            let error = GlobPattern::compile(&bad).err();
            assert!(
                matches!(
                    (absolute, &error),
                    (true, Some(FilterError::AbsolutePath { .. }))
                        | (false, Some(FilterError::Traversal { .. }))
                ),
                "seed {seed}: {bad:?} gave {error:?}"
            );
        }
    }
    assert!(
        compiled > 0 && matched > 0,
        "compiled {compiled}, matched {matched}"
    );
}

#[test]
fn effective_filters_are_the_conjunction_of_their_parts_in_any_order() {
    const LANGUAGES: &[&str] = &["rust", "python", "go", "typescript"];
    let (mut kept, mut dropped) = (0, 0);
    for seed in 0..FILTER_SEEDS {
        let mut rng = Rng(seed);
        let list = |rng: &mut Rng| -> Option<Vec<String>> {
            (rng.below(3) != 0).then(|| (0..rng.below(3) + 1).map(|_| pattern(rng)).collect())
        };
        let request = FilterRequest {
            languages: (rng.below(3) == 0).then(|| vec![rng.pick(LANGUAGES).to_string()]),
            include_paths: list(&mut rng),
            exclude_paths: list(&mut rng),
            source_roles: (rng.below(3) == 0)
                .then(|| vec![rng.pick(&SourceRole::ALL).as_str().to_owned()]),
        };
        let filter = EffectiveFilter::from_request(&request)
            .unwrap_or_else(|error| panic!("seed {seed}: {request:?}: {error}"));
        // The same lists shuffled and with repeats.
        let mut shuffled = request.clone();
        for list in [&mut shuffled.include_paths, &mut shuffled.exclude_paths]
            .into_iter()
            .flatten()
        {
            let repeat = list[rng.below(list.len())].clone();
            list.push(repeat);
            rng.shuffle(list);
        }
        let reordered = EffectiveFilter::from_request(&shuffled).unwrap();
        assert_eq!(filter.report(), reordered.report(), "seed {seed}");
        // One more include widens, one more exclude narrows.
        let extra = pattern(&mut rng);
        let mut wider = request.clone();
        if let Some(list) = &mut wider.include_paths {
            list.push(extra.clone());
        }
        let mut narrower = request.clone();
        narrower
            .exclude_paths
            .get_or_insert_with(Vec::new)
            .push(extra.clone());
        let wider = EffectiveFilter::from_request(&wider).unwrap();
        let narrower = EffectiveFilter::from_request(&narrower).unwrap();

        let globs = |list: &Option<Vec<String>>| {
            list.as_ref().map(|list| {
                list.iter()
                    .map(|p| GlobPattern::compile(p).unwrap())
                    .collect::<Vec<_>>()
            })
        };
        let (includes, excludes) = (globs(&request.include_paths), globs(&request.exclude_paths));
        for _ in 0..8 {
            let path = path(&mut rng);
            let spelled = respell(&mut rng, &path);
            let language = *rng.pick(LANGUAGES);
            let role = *rng.pick(&SourceRole::ALL);
            let expected = request
                .languages
                .as_ref()
                .is_none_or(|l| l.iter().any(|l| l == language))
                && request
                    .source_roles
                    .as_ref()
                    .is_none_or(|r| r.iter().any(|r| r == role.as_str()))
                && includes
                    .as_ref()
                    .is_none_or(|g| g.iter().any(|g| g.matches(&path)))
                && excludes
                    .as_ref()
                    .is_none_or(|g| !g.iter().any(|g| g.matches(&path)));
            let context = format!("seed {seed}: {request:?} on {language} {role:?} {spelled:?}");
            assert_eq!(filter.matches(language, &path, role), expected, "{context}");
            assert_eq!(
                filter.matches(language, &spelled, role),
                expected,
                "{context}"
            );
            assert_eq!(
                reordered.matches(language, &spelled, role),
                expected,
                "{context}"
            );
            if expected {
                assert!(
                    wider.matches(language, &path, role),
                    "{context} + {extra:?}"
                );
                kept += 1;
            } else {
                assert!(
                    !narrower.matches(language, &path, role),
                    "{context} - {extra:?}"
                );
                dropped += 1;
            }
            if narrower.matches(language, &path, role) {
                assert!(expected, "{context} - {extra:?}");
            }
        }
    }
    assert!(kept > 0 && dropped > 0, "kept {kept}, dropped {dropped}");
}
