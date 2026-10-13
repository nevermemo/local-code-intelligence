//! N59 (d): snippet budgets. A seeded loop builds text of one- to four-byte
//! characters and checks that `head_within` (the reranker document head) and
//! `store::bounded_snippet` (the evidence snippet) keep a valid prefix within
//! their budget and that the prefix is maximal: the next character would not
//! fit.

use super::head_within;
use crate::store::{SNIPPET_BUDGET, bounded_snippet};

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
}

/// One-, two-, three- and four-byte characters, and a combining mark.
const CHARACTERS: &[char] = &[
    'a', ' ', '\n', '{', 'é', 'ж', '€', '…', '中', '🦀', '\u{301}',
];

/// Text of about `bytes` bytes.
fn random_text(rng: &mut Rng, bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes {
        out.push(CHARACTERS[rng.below(CHARACTERS.len())]);
    }
    out
}

/// `head` is a prefix of `text` within `max` bytes, and either all of `text`
/// or so long that `text`'s next character would not fit.
fn assert_maximal_prefix(text: &str, head: &str, max: usize, context: &str) {
    // A `&str` prefix always ends on a character boundary.
    assert!(text.starts_with(head), "{context}: not a prefix");
    if text.len() <= max {
        assert_eq!(head, text, "{context}: cut although it fits");
        return;
    }
    assert!(
        head.len() <= max,
        "{context}: {} bytes over {max}",
        head.len()
    );
    let next = text[head.len()..].chars().next().unwrap();
    assert!(
        head.len() + next.len_utf8() > max,
        "{context}: {next:?} still fits after {} bytes",
        head.len()
    );
}

#[test]
fn snippet_heads_are_maximal_valid_prefixes_within_the_budget() {
    const SEEDS: u64 = 4000;
    // `snippet_mid_character`: snippets whose budget falls inside a
    // character, the case the row is about.
    let (mut cut, mut cut_mid_character, mut snippet_mid_character) = (0, 0, 0);
    for seed in 0..SEEDS {
        let mut rng = Rng(seed);
        // Around the snippet budget half of the time, short otherwise.
        let size = if rng.below(2) == 0 {
            SNIPPET_BUDGET - 8 + rng.below(16)
        } else {
            rng.below(40)
        };
        let text = random_text(&mut rng, size);

        for max in [
            0,
            1,
            2,
            3,
            rng.below(text.len() + 4),
            text.len(),
            SNIPPET_BUDGET,
        ] {
            let head = head_within(&text, max);
            assert_maximal_prefix(&text, head, max, &format!("seed {seed}, max {max}"));
            if head.len() < text.len() {
                cut += 1;
                cut_mid_character += usize::from(!text.is_char_boundary(max));
            }
        }

        let snippet = bounded_snippet(&text);
        let context = format!("seed {seed}, snippet");
        if text.len() <= SNIPPET_BUDGET {
            assert_eq!(snippet, text, "{context}");
        } else {
            // The kept prefix, then the ellipsis that marks the cut.
            let kept = snippet
                .strip_suffix('…')
                .unwrap_or_else(|| panic!("{context}: no ellipsis"));
            assert_maximal_prefix(&text, kept, SNIPPET_BUDGET, &context);
            assert_eq!(kept, head_within(&text, SNIPPET_BUDGET), "{context}");
            snippet_mid_character += usize::from(!text.is_char_boundary(SNIPPET_BUDGET));
        }
    }
    assert!(
        cut > 0 && cut_mid_character > 0 && snippet_mid_character > 0,
        "cut {cut}, cut inside a character {cut_mid_character}, snippets cut inside a \
         character {snippet_mid_character}"
    );
}
