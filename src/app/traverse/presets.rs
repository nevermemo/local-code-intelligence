//! Named steering policies and the effective policy of a call.
//!
//! A preset is a file under `presets/`: `{"description": .., "steer": {..},
//! "traverse": {..}}`. `steer` has the shape of the `steer` argument (rules,
//! ladder, max_stops, tau, stop_every, on_disagreement, likely_complete);
//! `traverse` holds the measured first-stage and walk settings of the question
//! type ([`policy_for`]). A call selects one with `steer.policy` (`--policy
//! NAME` on the CLI). Fields given beside the name replace the preset's. A
//! call that gives no policy field at all runs the first preset, `explore`,
//! whose steer is empty and whose traverse block is empty: what the engine ran
//! before presets existed. The policy is the router: the caller's intent picks
//! the walk, the maps and the k's; nothing reads the question text to pick one
//! (d391).
//!
//! [`effective`] describes what a call ran after the defaults: the stops,
//! rules, ladder and initial map weights, with the preset name and a hash of
//! that content. The response carries all of it; the `start` event of the
//! trace carries the name and the hash.

use super::SteerArgs;
use anyhow::{Result, anyhow};
use lci_core::config::{TraverseConfig, TraversePolicy};
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
    /// The first-stage and walk settings of the question type (see
    /// [`policy_for`]); none: the `[traverse]` keys alone.
    #[serde(default)]
    traverse: TraversePolicy,
}

/// The presets, the default first.
pub(super) const FILES: [(&str, &str); 5] = [
    ("explore", include_str!("presets/explore.json")),
    ("impact", include_str!("presets/impact.json")),
    ("flow", include_str!("presets/flow.json")),
    ("where-to", include_str!("presets/where-to.json")),
    ("history", include_str!("presets/history.json")),
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

/// What the call's policy sets about the first stage and the walk: the
/// `[traverse.policies.<name>]` keys over the preset file's `traverse` block
/// (the measured defaults). A call without a named policy (`None`) sets
/// nothing, so the `[traverse]` keys alone apply.
pub fn policy_for(limits: &TraverseConfig, preset: Option<&str>) -> Result<TraversePolicy> {
    let Some(name) = preset else {
        return Ok(TraversePolicy::default());
    };
    let built_in = load(name)?.traverse;
    Ok(match limits.policies.get(name) {
        Some(own) => own.over(&built_in),
        None => built_in,
    })
}

/// The maps of a continuation as it runs them: the maps as registered, with the
/// weights and on/off switches its handle replays (in order, the last one
/// wins). A map the handle names that this call has not registered is left
/// out, and a map the call registered off stays off whatever the handle says.
pub fn restored(
    maps: &[PlanMap],
    weights: &[(String, f64)],
    switches: &[(String, bool)],
) -> Vec<PlanMap> {
    let mut restored = maps.to_vec();
    for (id, weight) in weights {
        if let Some(map) = restored.iter_mut().find(|map| map.id == *id) {
            map.weight = *weight;
        }
    }
    for (id, on) in switches {
        if let Some((map, registered)) =
            restored.iter_mut().zip(maps).find(|(map, _)| map.id == *id)
        {
            map.enabled = *on && registered.enabled;
        }
    }
    restored
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
        assert_eq!(names, ["explore", "impact", "flow", "where-to", "history"]);
        assert_eq!(names, lci_core::config::TRAVERSE_POLICY_NAMES);
        assert!(is_preset("where-to") && !is_preset("default"));
    }

    #[test]
    fn the_presets_carry_the_measured_first_stage_and_walk_defaults() {
        let limits = TraverseConfig::default();
        let of = |name: &str| policy_for(&limits, Some(name)).unwrap();
        let maps = |names: &[&str]| Some(names.iter().map(|n| (*n).to_owned()).collect());
        let impact = of("impact");
        assert_eq!(impact.walk, Some(false));
        assert_eq!(impact.extra_maps, maps(&["exact.closure"]));
        assert_eq!((impact.first_k, impact.seed_k), (Some(20), Some(20)));
        let history = of("history");
        assert_eq!(history.walk, Some(false));
        assert_eq!(history.extra_maps, maps(&["git.history", "exact.pickaxe"]));
        assert_eq!((history.first_k, history.seed_k), (Some(20), Some(20)));
        assert_eq!(of("flow").walk, Some(true));
        assert_eq!(of("where-to").walk_min_answers, Some(20));
        // Unset or explore: the [traverse] keys alone.
        assert_eq!(of("explore"), TraversePolicy::default());
        assert_eq!(
            policy_for(&limits, None).unwrap(),
            TraversePolicy::default()
        );
        assert!(policy_for(&limits, Some("nope")).is_err());
        // The config table wins field by field.
        let mut own = TraverseConfig::default();
        own.policies.insert(
            "impact".into(),
            TraversePolicy {
                walk: Some(true),
                first_k: Some(12),
                ..TraversePolicy::default()
            },
        );
        let merged = policy_for(&own, Some("impact")).unwrap();
        assert_eq!(merged.walk, Some(true));
        assert_eq!(merged.first_k, Some(12));
        assert_eq!(merged.seed_k, Some(20));
        assert_eq!(merged.extra_maps, maps(&["exact.closure"]));
        // Every built-in table passes the config's own checks.
        for (name, _) in FILES {
            of(name).validate(name).unwrap();
        }
    }

    #[test]
    fn each_intent_picks_its_walk_and_its_maps() {
        fn names(list: &[&str]) -> Option<Vec<String>> {
            Some(list.iter().map(|n| (*n).to_owned()).collect())
        }
        let limits = TraverseConfig::default();
        let table = [
            ("explore", None, None, None),
            ("impact", Some(false), None, names(&["exact.closure"])),
            ("flow", Some(true), None, None),
            ("where-to", None, Some(20), None),
            (
                "history",
                Some(false),
                None,
                names(&["git.history", "exact.pickaxe"]),
            ),
        ];
        for (name, walk, walk_min_answers, extra_maps) in table {
            let picked = policy_for(&limits, Some(name)).unwrap();
            assert_eq!(picked.walk, walk, "{name} walk");
            assert_eq!(
                picked.walk_min_answers, walk_min_answers,
                "{name} walk_min_answers"
            );
            assert_eq!(picked.extra_maps, extra_maps, "{name} extra_maps");
        }
        // The config table routes an intent to grep or to a language server by
        // naming the opt-in map. It replaces the preset's list whole (`over`
        // takes the whole Option): impact then no longer names exact.closure.
        let mut own = TraverseConfig::default();
        own.policies.insert(
            "flow".into(),
            TraversePolicy {
                extra_maps: names(&["grep.live"]),
                ..TraversePolicy::default()
            },
        );
        own.policies.insert(
            "impact".into(),
            TraversePolicy {
                extra_maps: names(&["lsp.live"]),
                ..TraversePolicy::default()
            },
        );
        let flow = policy_for(&own, Some("flow")).unwrap();
        assert_eq!(flow.extra_maps, names(&["grep.live"]));
        assert_eq!(flow.walk, Some(true));
        let impact = policy_for(&own, Some("impact")).unwrap();
        assert_eq!(impact.extra_maps, names(&["lsp.live"]));
        assert_eq!(impact.walk, Some(false));
        // An intent without a table entry keeps its preset's pick.
        assert_eq!(
            policy_for(&own, Some("history")).unwrap().extra_maps,
            names(&["git.history", "exact.pickaxe"])
        );
    }

    #[test]
    fn a_continuations_maps_carry_the_weights_and_switches_of_its_handle() {
        let plan = |id: &str, weight: f64, enabled: bool| PlanMap {
            id: id.to_owned(),
            version: 1,
            weight,
            enabled,
        };
        let maps = [
            plan("m1", 1.0, true),
            plan("m2", 1.0, true),
            plan("m3", 1.0, false),
        ];
        let weights = [
            ("m1".to_owned(), 0.5),
            ("gone".to_owned(), 2.0),
            ("m1".to_owned(), 0.25),
        ];
        let switches = [
            ("m2".to_owned(), false),
            ("gone".to_owned(), false),
            ("m3".to_owned(), true),
            ("m2".to_owned(), true),
            ("m2".to_owned(), false),
        ];
        let restored = restored(&maps, &weights, &switches);
        assert_eq!(
            restored,
            [
                plan("m1", 0.25, true),
                plan("m2", 1.0, false),
                // Switched on by the handle, but registered off by this call.
                plan("m3", 1.0, false),
            ]
        );
        // Nothing to replay: the maps as registered.
        assert_eq!(super::restored(&maps, &[], &[]), maps);
        // The policy shown for the two differs in its map block and its hash.
        let steer = SteerArgs::default();
        let (fresh, fresh_plan) = effective(None, &steer, None, &maps);
        let (resumed, resumed_plan) = effective(None, &steer, None, &restored);
        assert_eq!(resumed["maps"][0]["weight"], 0.25);
        assert_eq!(resumed["maps"][1]["enabled"], false);
        assert_eq!(fresh["maps"][1]["enabled"], true);
        assert_ne!(fresh_plan.hash, resumed_plan.hash);
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
                .contains("explore, impact, flow, where-to, history"),
            "{error}"
        );
    }
}
