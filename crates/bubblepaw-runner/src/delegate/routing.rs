//! Explicit routes with in-class exploration and a seedable generator.
use super::assignment::{Assignment, Route};
use crate::config::DelegateSettings;
use anyhow::{Result, anyhow, ensure};

/// SplitMix64: small, seedable and good enough for an explore share.
#[derive(Clone, Debug)]
pub struct Rng(u64);
impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Self(seed)
    }
    /// Seed from the clock and process id when no seed is given.
    pub fn from_entropy() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();
        Self(nanos ^ (u64::from(std::process::id()) << 32))
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.unit() * n as f64) as usize % n.max(1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteChoice {
    /// `provider/model` that runs.
    pub model: String,
    /// The class, for a class route.
    pub class: Option<String>,
    /// named, default, explore, reframe or escalate.
    pub reason: String,
}

/// Share of class routes sent to a uniformly chosen non-default candidate.
pub const EXPLORE_SHARE: f64 = 0.15;

/// Choose the model for a delegation. Follow-ups record their own reason.
pub fn choose(
    assignment: &Assignment,
    settings: &DelegateSettings,
    rng: &mut Rng,
) -> Result<RouteChoice> {
    choose_with(assignment, settings, EXPLORE_SHARE, rng)
}

fn choose_with(
    assignment: &Assignment,
    settings: &DelegateSettings,
    explore: f64,
    rng: &mut Rng,
) -> Result<RouteChoice> {
    let follow_up = assignment
        .parent
        .as_ref()
        .map(|(_, reason)| reason.as_str());
    match &assignment.route {
        Route::Model(model) => Ok(RouteChoice {
            model: model.clone(),
            class: None,
            reason: follow_up.unwrap_or("named").to_owned(),
        }),
        Route::Class(class) => {
            let entry = settings.classes.get(class).ok_or_else(|| {
                anyhow!(
                    "route class `{class}` is not configured in delegate.classes ({})",
                    settings
                        .classes
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            let others: Vec<_> = entry
                .candidates
                .iter()
                .filter(|candidate| *candidate != &entry.default)
                .collect();
            // Follow-ups are never explored: their data stays apart from explore data.
            let (model, reason) = if let Some(reason) = follow_up {
                (entry.default.clone(), reason)
            } else if !others.is_empty() && rng.unit() < explore {
                (others[rng.below(others.len())].clone(), "explore")
            } else {
                (entry.default.clone(), "default")
            };
            ensure!(!model.is_empty(), "delegate class `{class}` has no default");
            Ok(RouteChoice {
                model,
                class: Some(class.clone()),
                reason: reason.to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DelegateSettings, ModelClass};

    fn settings() -> DelegateSettings {
        DelegateSettings {
            classes: [(
                "small".to_owned(),
                ModelClass {
                    default: "a/one".into(),
                    candidates: vec!["a/two".into(), "a/three".into()],
                },
            )]
            .into(),
        }
    }
    fn assignment(route: &str, parent: &str) -> Assignment {
        Assignment::parse(&format!(
            "+++\ncategory = \"fix\"\nroute = {route}\n{parent}+++\nDo it.\n"
        ))
        .unwrap()
    }

    #[test]
    fn seeded_routing_explores_its_share_uniformly_within_the_class() {
        let class = assignment("{ class = \"small\" }", "");
        let mut rng = Rng::seeded(7);
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for _ in 0..4000 {
            let choice = choose(&class, &settings(), &mut rng).unwrap();
            assert_eq!(choice.class.as_deref(), Some("small"));
            match choice.reason.as_str() {
                "default" => assert_eq!(choice.model, "a/one"),
                "explore" => assert_ne!(choice.model, "a/one"),
                other => panic!("unexpected reason {other}"),
            }
            *counts.entry(choice.model).or_default() += 1;
        }
        let explored = 4000 - counts["a/one"];
        assert!((480..=720).contains(&explored), "{explored}");
        assert!(
            counts["a/two"].abs_diff(counts["a/three"]) < 120,
            "{counts:?}"
        );
        // The same seed reproduces the same choices.
        let a: Vec<_> = (0..50)
            .scan(Rng::seeded(3), |rng, _| {
                Some(choose_with(&class, &settings(), 0.5, rng).unwrap())
            })
            .collect();
        let b: Vec<_> = (0..50)
            .scan(Rng::seeded(3), |rng, _| {
                Some(choose_with(&class, &settings(), 0.5, rng).unwrap())
            })
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn named_and_follow_up_routes_are_never_explored() {
        let mut rng = Rng::seeded(1);
        let named = assignment("{ model = \"x/y\" }", "");
        let escalated = assignment(
            "{ class = \"small\" }",
            "parent = \"p\"\nreason = \"escalate\"\n",
        );
        let reframed = assignment(
            "{ model = \"x/y\" }",
            "parent = \"p\"\nreason = \"reframe\"\n",
        );
        for _ in 0..100 {
            let n = choose_with(&named, &settings(), 1.0, &mut rng).unwrap();
            assert_eq!((n.model.as_str(), n.reason.as_str()), ("x/y", "named"));
            let e = choose_with(&escalated, &settings(), 1.0, &mut rng).unwrap();
            assert_eq!((e.model.as_str(), e.reason.as_str()), ("a/one", "escalate"));
            let r = choose_with(&reframed, &settings(), 1.0, &mut rng).unwrap();
            assert_eq!(r.reason, "reframe");
        }
        let unknown = assignment("{ class = \"huge\" }", "");
        assert!(choose(&unknown, &settings(), &mut rng).is_err());
    }
}
