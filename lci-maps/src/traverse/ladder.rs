//! The steering ladder (spine brief 1.5): handlers tried in order at a stop,
//! the first confident decision applied, the outer agent when none is.
//!
//! | rung | handler | decides | uncertain when |
//! | --- | --- | --- | --- |
//! | 1 | [`RuleList`](super::rules::RuleList) | the agent's own rules | no rule matched, or two matched rules contradict (prune against expand or boost) |
//! | 2 | System One (slice S8; a slot, a test double until then) | the fuzzy re-rank | its own margin or a `defer` |
//! | 3 | the outer agent | anything | never; also where a budget ends the call |
//!
//! A [`Ladder`] is a [`Handler`]. [`Ladder::new`] takes the handlers of rungs
//! 1 and 2 in order (either may be absent) and `tau`: the first handler whose
//! decision has `confidence >= tau` and was not cut short decides, and the
//! rungs before it are recorded in the decision's trail as uncertain. When
//! none is confident the decision is `escalate`: the engine ends the call
//! `Stopped`, returns the checkpoint with a handle (rung 3), and the agent
//! sends its answer as a [`Steer`](super::steer::Steer) on the resume.
//!
//! Today the product builds the ladder with the rules rung alone
//! ([`Ladder::rules`]), whose confidence is 1.0 when a rule matched, the
//! list was not cut short and no two matched rules conflict, else 0.0. So
//! `tau` acts as a switch: every `tau` in `(0, 1]` behaves the same (the
//! rules decide exactly on a clean match, any other stop escalates), and
//! `tau = 0` lets an unmatched list "decide" with no actions, so no stop
//! escalates to the caller (a rule's own `stop` and a list cut short still
//! end the call). A graded `tau` needs a rung whose
//! confidence is graded (System One).
//!
//! Nothing of the lower rungs is applied when the ladder escalates: a rung
//! that was not confident does not get half its way.

use super::handler::{
    Attempt, Checkpoint, Decision, Handler, RUNG_AGENT, RUNG_SYSTEM_ONE, StopBudget,
};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LadderError {
    /// `tau` is not a number from 0 to 1.
    BadTau,
    /// A handler sits on a rung other than 1 or 2, or the rungs are not in
    /// ascending order without repeats.
    BadRungs(Vec<u8>),
}

impl std::fmt::Display for LadderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadTau => write!(f, "tau must be a number from 0 to 1"),
            Self::BadRungs(rungs) => write!(
                f,
                "the ladder takes handlers of rung 1 and 2 in ascending order, got {rungs:?}"
            ),
        }
    }
}

impl std::error::Error for LadderError {}

/// The handlers of rungs 1 and 2, and the confidence they must reach.
pub struct Ladder {
    rungs: Vec<Box<dyn Handler>>,
    tau: f32,
}

impl Ladder {
    pub fn new(tau: f32, rungs: Vec<Box<dyn Handler>>) -> Result<Self, LadderError> {
        if !(0.0..=1.0).contains(&tau) {
            return Err(LadderError::BadTau);
        }
        let numbers: Vec<u8> = rungs.iter().map(|h| h.rung()).collect();
        let ascending = numbers.windows(2).all(|w| w[0] < w[1]);
        if !ascending || numbers.iter().any(|&r| r == 0 || r >= RUNG_AGENT) {
            return Err(LadderError::BadRungs(numbers));
        }
        Ok(Self { rungs, tau })
    }

    /// Rung 1 only (the common case), or none.
    pub fn rules(tau: f32, rules: impl Handler + 'static) -> Result<Self, LadderError> {
        Self::new(tau, vec![Box::new(rules)])
    }

    /// Rungs 1 and 2: the slot S8 fills with the model-backed handlers.
    pub fn with_system_one(
        tau: f32,
        rules: impl Handler + 'static,
        system_one: impl Handler + 'static,
    ) -> Result<Self, LadderError> {
        debug_assert_eq!(system_one.rung(), RUNG_SYSTEM_ONE);
        Self::new(tau, vec![Box::new(rules), Box::new(system_one)])
    }

    pub fn tau(&self) -> f32 {
        self.tau
    }

    pub fn len(&self) -> usize {
        self.rungs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rungs.is_empty()
    }
}

impl Handler for Ladder {
    fn id(&self) -> &str {
        "ladder"
    }

    fn decide(&self, checkpoint: &Checkpoint, budget: &StopBudget) -> Decision {
        let mut trail = Vec::new();
        let mut spent = Duration::ZERO;
        for handler in &self.rungs {
            let decision = handler.decide(checkpoint, budget);
            spent += decision.cost;
            let confident = decision.confidence.is_finite()
                && decision.confidence >= self.tau
                && !decision.truncated;
            if confident {
                return Decision {
                    cost: spent,
                    rung: handler.rung(),
                    handler: handler.id().to_string(),
                    trail,
                    escalate: false,
                    ..decision
                };
            }
            trail.push(Attempt {
                rung: handler.rung(),
                handler: handler.id().to_string(),
                confidence: decision.confidence,
                truncated: decision.truncated,
                actions: decision.actions,
            });
        }
        Decision {
            actions: Vec::new(),
            confidence: 0.0,
            cost: spent,
            truncated: false,
            rung: RUNG_AGENT,
            handler: "agent".to_string(),
            escalate: true,
            trail,
        }
    }
}
