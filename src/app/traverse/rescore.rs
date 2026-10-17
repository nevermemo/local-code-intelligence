//! The reranker frontier's rescorer: rates the destinations of an expansion
//! with the configured reranker.

use crate::models::Models;
use anyhow::Result;
use lci_core::{config::TraverseRerankScores, models::HttpStatusError, node::NodeId};
use lci_maps::{
    scip_graph::ScipMap,
    traverse::rescore::{FrontierRescorer, RerankFailure, RescoreError},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
};

/// The longest text the reranker frontier sends for one node, in characters.
pub(super) const RERANK_TEXT_CHARS: usize = 1000;

/// The reranker frontier's rescorer: rates the destinations of an expansion
/// with the configured reranker (`Models::rerank`, one batched call). The text
/// of a node is what the call already holds without a new index query: for a
/// SCIP symbol its file, kind and `Owner::name` from the graph in memory, for
/// a file its path, for any other node its id (a chunk's id names its file).
/// Not the source text.
pub(super) struct GraphRescorer<'a> {
    models: &'a Models,
    scip: Arc<ScipMap>,
    by_node: HashMap<NodeId, usize>,
    /// What the reranker's scores mean (`[traverse] rerank_scores`).
    scores: TraverseRerankScores,
    /// The reranker's model and endpoint as text for a handle, without the
    /// scores setting (see [`FrontierRescorer::identity`]).
    reranker: String,
    /// The error of every call that failed, in order.
    errors: Mutex<Vec<String>>,
    /// Scores that lay outside `[0, 1]` and were clamped (probability reading).
    out_of_range: AtomicU32,
}

impl<'a> GraphRescorer<'a> {
    /// `reranker` is [`SeedSearch::reranker_identity`] (none: the search does
    /// not say, and the handle cannot tell one reranker from another).
    pub(super) fn new(
        models: &'a Models,
        scip: Arc<ScipMap>,
        reranker: Option<String>,
        scores: TraverseRerankScores,
    ) -> Self {
        let by_node = (0..scip.graph().nodes.len())
            .map(|i| (scip.node_id(i).clone(), i))
            .collect();
        Self {
            models,
            scip,
            by_node,
            scores,
            reranker: reranker.unwrap_or_else(|| "reranker of unknown model".to_owned()),
            errors: Mutex::default(),
            out_of_range: AtomicU32::new(0),
        }
    }

    /// How many scores were clamped into `[0, 1]` over this call.
    pub(super) fn clamped(&self) -> u32 {
        self.out_of_range.load(Ordering::Relaxed)
    }

    pub(super) fn text(&self, node: &NodeId) -> String {
        let text = match (self.by_node.get(node), node) {
            (Some(&i), _) => {
                let n = &self.scip.graph().nodes[i];
                let kind = match n.kind {
                    lci_maps::scip_graph::FnKind::Function => "fn",
                    lci_maps::scip_graph::FnKind::Method => "method",
                };
                format!("{}\n{kind} {}", n.file.replace('\\', "/"), n.name)
            }
            (None, NodeId::File { path }) => path.clone(),
            (None, other) => other.to_string(),
        };
        text.chars().take(RERANK_TEXT_CHARS).collect()
    }

    pub(super) fn errors(&self) -> Vec<String> {
        self.errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One reranker score as a relevance in `[0, 1]`, by what the score means and
/// from that document's score alone: a probability is used as it is (clamped
/// into the range), a logit goes through the logistic function
/// `1 / (1 + e^-x)`. The second value is whether a probability lay outside
/// `[0, 1]`. Whatever the other documents of the call scored, the same score
/// gives the same relevance.
pub(super) fn relevance(score: f64, scores: TraverseRerankScores) -> (f64, bool) {
    match scores {
        TraverseRerankScores::Probability => (
            score.clamp(0.0, 1.0),
            score.is_finite() && !(0.0..=1.0).contains(&score),
        ),
        TraverseRerankScores::Logit => (1.0 / (1.0 + (-score).exp()), false),
    }
}

/// Why a reranker call failed, from the error `Models::rerank` returned: an
/// error status, a timeout, a service that cannot be reached (or is skipped
/// as recently unreachable), an answer that cannot be read or holds the wrong
/// number of scores, any other.
pub(super) fn classify(error: &anyhow::Error) -> RerankFailure {
    if error.downcast_ref::<HttpStatusError>().is_some() {
        return RerankFailure::Status;
    }
    let request = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>());
    if let Some(request) = request {
        if request.is_timeout() {
            return RerankFailure::Timeout;
        }
        if request.is_connect() {
            return RerankFailure::Unreachable;
        }
    }
    let text = format!("{error:#}");
    if text.contains("recently unreachable") {
        RerankFailure::Unreachable
    } else if text.contains("count mismatch") {
        RerankFailure::CountMismatch
    } else if text.contains("invalid reranker") || text.contains("byte limit") {
        RerankFailure::Malformed
    } else {
        RerankFailure::Other
    }
}

#[lci_maps::async_trait]
impl FrontierRescorer for &GraphRescorer<'_> {
    fn identity(&self) -> String {
        format!("{} scores={}", self.reranker, self.scores.as_str())
    }

    async fn relevance(&self, question: &str, nodes: &[NodeId]) -> Result<Vec<f64>, RescoreError> {
        let answer = if question.trim().is_empty() {
            Err(RescoreError::new(
                RerankFailure::Other,
                "the question is empty",
            ))
        } else {
            let documents: Vec<String> = nodes.iter().map(|node| self.text(node)).collect();
            self.models
                .rerank(question, &documents)
                .await
                .map_err(|error| RescoreError::new(classify(&error), format!("{error:#}")))
        };
        let ranks = match answer {
            Ok(ranks) => ranks,
            Err(error) => {
                self.errors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(error.to_string());
                return Err(error);
            }
        };
        let mut scores = vec![f64::NAN; nodes.len()];
        for (index, score) in ranks {
            scores[index] = f64::from(score);
        }
        let mut clamped = 0;
        let rated = scores
            .iter()
            .map(|&score| {
                let (relevance, outside) = relevance(score, self.scores);
                clamped += u32::from(outside);
                relevance
            })
            .collect();
        self.out_of_range.fetch_add(clamped, Ordering::Relaxed);
        Ok(rated)
    }
}
