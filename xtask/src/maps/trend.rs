//! The trend file: one markdown table row per (wave, set), appended by
//! `cargo xtask maps bench`. Rows are append-only history, so a second row
//! for the same (wave, set) is refused; run the next wave under a new label.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// Written once, when the file does not exist yet.
const PREFACE: &str = "\
# Multimap trend

One row per (wave, set), appended by `cargo xtask maps bench` (see
`xtask/src/maps/bench.rs`). Rows are never rewritten: a new measurement is a
new wave label. The gold sets and the frozen tree are private; they are passed
to the runner as `<gold>` and `<tree>` and nothing here names their contents.

How to read a row:

- `n_gold` is the number of gold items in the run (impact: the `primary` items
  only, cut by `--limit`). `n_attempted` is how many got a traversal call;
  `n_scored` is how many returned an answer to score. `skipped` counts the
  rest by cause: `timeout` (the call ran past the per-item limit and was
  killed), `cli_error` (the CLI failed or printed no JSON),
  `no_answer_type` (no selected map emits the node kind the set needs) and
  `map_unavailable` (the preflight call for the map failed, so the run did not
  use it). Nothing is dropped without a count: n_gold = n_scored + skipped
  (+ the items of a set that was not attempted at all).
- Every metric, precision, F0.5 and total below is over the `n_scored` items
  only; a skipped item is in no mean. `non-empty` is how many of the scored
  items returned at least one answer of the set's type: it counts answers, not
  correct answers (the metrics say how right they are). An item that returns
  nothing scores 0, including in precision.
- `metrics` are the gold scorer's own metrics for the set (means over the
  scored items where defined; `(k)` marks a metric defined on only k items).
- `precision` and `F0.5` are of the returned answer set against gold, per item,
  then averaged. F0.5 weighs precision twice as much as recall, because
  low-precision context costs the agent that reads it. The per-item precision
  and recall come from the scorer's own definitions (impact: declaration
  precision and recall; flow: answers that are not spurious, and path recall;
  where-to: set precision and recall, and for registration items the distinct
  non-neutral files against the expected files; history: accepted commits among
  the distinct commits returned, and whether one was found; instructions:
  section precision and recall for I1, required-unit precision@5 and recall
  for I2).
- `pops`, `hops`, `xhops` (cross-map hops) and `stops` are sums over the scored
  items of the traversal's own totals. `ans tok` is the sum of characters of the
  answer text the traversal returned, divided by 4 (an approximation, not a
  tokenizer count).
- `engine ms` is the traversal's own elapsed time per item, p50/p95;
  `wall ms` is the whole `lci traverse` process per item (it loads the index
  each time), p50/p95. A latency marked `under load` was measured while other
  work ran on the machine and is not comparable with a clean run.
- `maps` are the maps asked for (when none were, the list the CLI itself turns
  on); `maps unavailable` lists those whose preflight failed (they were not
  used). The preflight gives each map start nodes it can expand, then checks
  that the CLI registered the map and that the per-map totals show it was
  called; a seeds map is checked by the start nodes it gave.
- A set no selected map can answer (no selected map emits the node kind its
  answers are made of) gets `no map yields this answer type` instead of zeros,
  and the note `structural-zero`: the zero is the shape of the map list, not a
  result, so such a row is left out of any average over sets or waves. This
  file computes no average; a reader that does must skip those rows.
- A note that starts with `UNRELIABLE, not decision-grade` says some scored
  items stopped on the deadline, had reranker failures or had map call
  failures (with the counts of items); such a row must not decide anything.
  `maps compare` leaves those items out of a paired comparison. A note
  `expansions=N (pops P + pass-through pops Q)` gives the total expansions of a
  `--pass-through` run, whose `pops` column omits the pass-through pops. A
  note `salt=...` after `holdout-only` is the split salt of the prior table.
- A row whose note says `rescored` was scored again from the raw outputs of an
  earlier run with the current adapters (`--rescore`); its traversal columns
  are the earlier run's and its wall latency is `-`.

";

const COLUMNS: [&str; NCOLS] = [
    "wave",
    "set",
    "n_gold",
    "n_attempted",
    "n_scored",
    "skipped",
    "non-empty",
    "metrics (over n_scored)",
    "precision",
    "F0.5",
    "pops",
    "hops",
    "xhops",
    "stops",
    "ans tok",
    "engine ms p50/p95",
    "wall ms p50/p95",
    "maps",
    "maps unavailable",
    "policy",
    "note",
];

/// The number of columns.
pub const NCOLS: usize = 21;

/// One trend row: the cells in column order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendRow {
    pub cells: [String; NCOLS],
}

impl TrendRow {
    pub fn wave(&self) -> &str {
        &self.cells[0]
    }

    pub fn set(&self) -> &str {
        &self.cells[1]
    }

    /// `| a | b | ... |`, with `|` and newlines in a cell made harmless.
    pub fn to_markdown(&self) -> String {
        let cells: Vec<String> = self
            .cells
            .iter()
            .map(|c| c.replace('|', "/").replace(['\n', '\r'], " "))
            .collect();
        format!("| {} |", cells.join(" | "))
    }
}

fn header() -> String {
    let names = COLUMNS.join(" | ");
    let rule = COLUMNS
        .iter()
        .map(|_| "---")
        .collect::<Vec<_>>()
        .join(" | ");
    format!("| {names} |\n| {rule} |\n")
}

/// The (wave, set) pairs already in `text`.
fn existing_keys(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| l.starts_with("| "))
        .filter_map(|l| {
            let mut cells = l.trim_matches(|c| c == '|' || c == ' ').split(" | ");
            Some((cells.next()?.to_string(), cells.next()?.to_string()))
        })
        .filter(|(w, s)| w != "wave" && !w.starts_with("---") && !s.starts_with("---"))
        .collect()
}

/// Appends `row`, creating the file (preface and header) when it is missing.
/// Refuses a second row for the same (wave, set).
pub fn append(path: &Path, row: &TrendRow) -> Result<()> {
    let mut text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            format!("{PREFACE}{}", header())
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let key = (row.wave().to_string(), row.set().to_string());
    if existing_keys(&text).contains(&key) {
        bail!(
            "{} already has a row for wave {:?} and set {:?}; use a new --wave label",
            path.display(),
            key.0,
            key.1
        );
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&row.to_markdown());
    text.push('\n');
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(wave: &str, set: &str) -> TrendRow {
        let mut cells: [String; NCOLS] = std::array::from_fn(|i| format!("c{i}"));
        cells[0] = wave.to_string();
        cells[1] = set.to_string();
        TrendRow { cells }
    }

    #[test]
    fn creates_the_file_with_header_then_appends_rows() {
        let dir = tempdir("trend-create");
        let path = dir.join("trend.md");
        append(&path, &row("w1", "impact")).unwrap();
        append(&path, &row("w1", "flow")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Multimap trend"));
        assert_eq!(text.matches("| wave | set |").count(), 1);
        let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("| w1 |")).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("| impact |") && rows[1].contains("| flow |"));
    }

    #[test]
    fn a_second_row_for_the_same_wave_and_set_is_refused() {
        let dir = tempdir("trend-dup");
        let path = dir.join("trend.md");
        append(&path, &row("w1", "impact")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let error = append(&path, &row("w1", "impact")).unwrap_err().to_string();
        assert!(error.contains("already has a row"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        append(&path, &row("w2", "impact")).unwrap();
    }

    #[test]
    fn the_answer_column_says_it_counts_answers_not_correct_ones() {
        assert!(COLUMNS.contains(&"non-empty"));
        assert!(!COLUMNS.contains(&"answered"));
        assert!(PREFACE.contains("it counts answers"));
        assert!(PREFACE.contains("structural-zero"));
    }

    #[test]
    fn pipes_and_newlines_cannot_break_a_row() {
        let mut r = row("w", "s");
        r.cells[4] = "a|b\nc".to_string();
        let line = r.to_markdown();
        assert!(!line.contains('\n'));
        assert_eq!(line.matches('|').count(), NCOLS + 1);
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
