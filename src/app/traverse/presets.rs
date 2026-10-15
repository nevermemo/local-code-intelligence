//! Named steering policies and the effective policy of a call.
//!
//! A preset is a file under `presets/`: `{"description": .., "steer": {..}}`,
//! where `steer` has the shape of the `steer` argument (rules, ladder,
//! max_stops, tau, stop_every, on_disagreement, likely_complete). A call
//! selects one with `steer.policy` (`--policy NAME` on the CLI). Fields given
//! beside the name replace the preset's. A call that gives no policy field at
//! all runs the first preset, `explore`, whose steer is empty: what the engine
//! ran before presets existed.
//!
//! [`effective`] describes what a call ran after the defaults: the stops,
//! rules, ladder and initial map weights, with the preset name and a hash of
//! that content. The response carries all of it; the `start` event of the
//! trace carries the name and the hash.

use super::SteerArgs;
use anyhow::{Result, anyhow};
use lci_maps::traverse::{
    handler::StopPolicy,
    trace::{PlanEffective, PlanMap, PlanPolicy, hash16},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetFile {
    /// Read by the tests: the file's own documentation.
    #[allow(dead_code)]
    description: String,
    steer: SteerArgs,
}

/// The presets, the default first.
pub(super) const FILES: [(&str, &str); 4] = [
    ("explore", include_str!("presets/explore.json")),
    ("impact", include_str!("presets/impact.json")),
    ("flow", include_str!("presets/flow.json")),
    ("where-to", include_str!("presets/where-to.json")),
];

/// Whether `name` is a preset.
pub fn is_preset(name: &str) -> bool {
    FILES.iter().any(|(known, _)| *known == name)
}

fn load(name: &str) -> Result<PresetFile> {
    let (_, text) = FILES
        .iter()
        .find(|(known, _)| *known == name)
        .ok_or_else(|| {
            let names: Vec<&str> = FILES.iter().map(|(known, _)| *known).collect();
            anyhow!(
                "steer.policy: unknown preset {name:?}; the presets are {}",
                names.join(", ")
            )
        })?;
    serde_json::from_str(text).map_err(|e| anyhow!("preset {name}: {e}"))
}

impl SteerArgs {
    /// Whether any field of the policy is given (the answer to a checkpoint,
    /// `actions`, is not part of it).
    fn sets_policy(&self) -> bool {
        self.rules.is_some()
            || self.script.is_some()
            || self.ladder.is_some()
            || self.max_stops.is_some()
            || self.tau.is_some()
            || self.stop_every.is_some()
            || self.on_disagreement.is_some()
            || self.likely_complete.is_some()
    }
}

/// The steer a call runs and the name of the preset it took: the named
/// preset with the call's own fields over it; `explore` when no policy field
/// is given; none for fields given without a name.
pub fn resolve(asked: &SteerArgs) -> Result<(SteerArgs, Option<String>)> {
    let Some(name) = &asked.policy else {
        let preset = (!asked.sets_policy()).then(|| FILES[0].0.to_owned());
        return Ok((asked.clone(), preset));
    };
    let base = load(name)?.steer;
    let steer = SteerArgs {
        policy: None,
        rules: asked.rules.clone().or(base.rules),
        script: asked.script.clone().or(base.script),
        ladder: asked.ladder.clone().or(base.ladder),
        max_stops: asked.max_stops.or(base.max_stops),
        tau: asked.tau.or(base.tau),
        stop_every: asked.stop_every.or(base.stop_every),
        on_disagreement: asked.on_disagreement.or(base.on_disagreement),
        likely_complete: asked.likely_complete.or(base.likely_complete),
        actions: asked.actions.clone(),
    };
    Ok((steer, Some(name.clone())))
}

/// The effective policy of a call: the response object and the part the
/// `start` event records. `handler` is the stop policy and plan the call
/// built, `None` for a call without a handler (no stops, no rules, no
/// ladder). `maps` are the maps as registered, with their initial weights.
pub fn effective(
    preset: Option<String>,
    steer: &SteerArgs,
    handler: Option<(&StopPolicy, &PlanPolicy)>,
    maps: &[PlanMap],
) -> (Value, PlanEffective) {
    let stops = handler.map(|(stops, _)| {
        json!({
            "every_pops": stops.every_pops,
            "max_stops": stops.max_stops,
            "budget_share": stops.budget_share,
            "on_disagreement": stops.on_disagreement,
            "on_stagnation": stops.on_stagnation,
            "likely_complete": stops.likely_complete,
        })
    });
    let plan = handler.map(|(_, plan)| plan);
    let mut content = json!({
        "stops": stops,
        "rules": steer.rules.clone().unwrap_or_else(|| json!([])),
        "ladder": plan.map_or_else(Vec::new, |plan| plan.ladder.clone()),
        // f32 to six places, so 0.8 is written 0.8.
        "tau": plan.and_then(|plan| plan.tau).map(|t| (f64::from(t) * 1e6).round() / 1e6),
        "maps": maps.iter().map(|map| json!({
            "id": map.id,
            "weight": map.weight,
            "enabled": map.enabled,
        })).collect::<Vec<_>>(),
    });
    let hash = hash16(content.to_string().as_bytes());
    if let Some(object) = content.as_object_mut() {
        object.insert("preset".into(), json!(preset));
        object.insert("hash".into(), json!(hash));
    }
    (content, PlanEffective { preset, hash })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_parses_and_the_untuned_ones_say_so() {
        for (name, _) in FILES {
            let preset = load(name).unwrap();
            assert!(!preset.description.is_empty(), "{name}");
            if name != "explore" {
                assert!(preset.description.contains("Untuned"), "{name}");
                assert!(preset.description.contains("tuned from traces"), "{name}");
            }
        }
        let names: Vec<&str> = FILES.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["explore", "impact", "flow", "where-to"]);
        assert!(is_preset("where-to") && !is_preset("default"));
    }

    #[test]
    fn no_policy_resolves_to_explore_and_changes_nothing() {
        let (steer, preset) = resolve(&SteerArgs::default()).unwrap();
        assert_eq!(preset.as_deref(), Some("explore"));
        assert_eq!(format!("{steer:?}"), format!("{:?}", SteerArgs::default()));
        // The first preset is that very steer.
        let (named, _) = resolve(&SteerArgs {
            policy: Some("explore".into()),
            ..SteerArgs::default()
        })
        .unwrap();
        assert_eq!(format!("{named:?}"), format!("{:?}", SteerArgs::default()));
    }

    #[test]
    fn fields_beside_a_name_replace_the_presets_and_the_actions_pass_through() {
        let asked = SteerArgs {
            policy: Some("impact".into()),
            stop_every: Some(7),
            actions: Some(json!([])),
            ..SteerArgs::default()
        };
        let (steer, preset) = resolve(&asked).unwrap();
        assert_eq!(preset.as_deref(), Some("impact"));
        assert_eq!((steer.stop_every, steer.policy), (Some(7), None));
        assert_eq!(steer.actions, Some(json!([])));
        // Fields without a name are a custom policy.
        let custom = SteerArgs {
            stop_every: Some(7),
            ..SteerArgs::default()
        };
        assert_eq!(resolve(&custom).unwrap().1, None);
        let error = resolve(&SteerArgs {
            policy: Some("nope".into()),
            ..SteerArgs::default()
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("explore, impact, flow, where-to"),
            "{error}"
        );
    }
}
