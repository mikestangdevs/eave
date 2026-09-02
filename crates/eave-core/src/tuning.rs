//! Per-session **tuning**: which model answers, how hard it thinks and
//! whether it runs on the provider's fast lane. Three knobs the operator
//! turns from the composer, stated per provider by the adapter that
//! drives it.
//!
//! The MODEL joined on 2026-09-02 (Slice TN2). Until then a session's
//! model was fixed at open (`AgentConfig::model`, in the journal header),
//! and picking another one in the composer started a NEW chat — the
//! operator: *"i can switch models mid conversation in claude cli
//! terminal and others. your telling me i cant in eave?"* The header's
//! model is still the one the session was OPENED with; the tuning's
//! model, when set, is the one it runs under now.
//!
//! | provider | model switch | how |
//! | --- | --- | --- |
//! | Claude Code 2.1.258 | live | `control_request { subtype: set_model, model }` (the same channel `interrupt` uses; answered by a `control_response`) |
//! | Codex 0.148.0 | live | `turn/start.model` (beside `effort` and `serviceTier`, per the schema) |
//! | Cursor 2026.08.11 | live | `session/set_model { sessionId, modelId }` — the frame the spawn already sends |
//! | Grok 1.0.13 | live | `session/set_model` (ACP; `session/new` lists `availableModels`) |
//! | Hermes 0.20.4 | **not offered** | its ACP adapter's method set has no `set_model` and `set_config_option` stores without reading |
//! | Pi 0.84.2 | live | RPC `{type: set_model, provider, modelId}` — Pi's ids are `provider/id`, split at the first `/` |
//!
//! # Why this is a table and not a flag
//!
//! Every CLI spells these differently and eave invents none of it:
//!
//! | provider | effort | speed | takes hold |
//! | --- | --- | --- | --- |
//! | Claude Code 2.1.258 | `--effort low\|medium\|high\|xhigh\|max` | `fastMode` in `--settings` | next spawn |
//! | Codex 0.148.0 | `turn/start.effort` (per model: low…ultra) | `turn/start.serviceTier: "priority"` | next turn |
//! | Cursor 2026.08.11 | `effort=` inside the model id | `fast=` inside the model id | **not offered** — the accepted words are only listed by `--list-models`, which needs a login this machine does not have |
//! | Grok 1.0.13 | `--reasoning-effort low\|medium\|high\|xhigh` | none | next spawn |
//! | Hermes 0.20.4 | `config.yaml` only — `hermes acp` takes no flag and its `set_config_option` stores without reading | none | **not offered** |
//! | Pi 0.84.2 | `--thinking off…max`, then `set_thinking_level` over RPC | none | next turn |
//!
//! All measured on 2026-09-02 from the installed binaries (`--help`,
//! `codex app-server generate-json-schema` + a live `model/list`, a live
//! Grok `session/new`, Pi's `rpc-mode.js`). The evidence strings on each
//! adapter's [`crate::provider::AgentAdapter::TUNING`] say which.
//!
//! # The rule for "when does it take hold"
//!
//! [`Applies::NextTurn`] providers change on the wire mid-conversation.
//! [`Applies::NextSpawn`] providers only read the value from argv, so eave
//! applies a change by **restarting the agent between turns** and
//! resuming the conversation by the provider's own cursor — the path the
//! engine already takes after an app restart. A turn in flight is never
//! interrupted for a knob; the engine waits for the next send that finds
//! no turn open. Both answers are stated back to the caller
//! (`TuneOk::takes_hold`), never assumed.
//!
//! # What eave does not do
//!
//! - It does not validate a level beyond the adapter's own measured list.
//!   An unknown word is refused at the engine with the list, so a typo
//!   cannot reach a CLI that would silently fall back.
//! - It does not remember a level per provider. A session's tuning is a
//!   fact ABOUT that session (`meta.json`, beside `title` and `archived`)
//!   and a fresh session starts on the provider's own default.

use serde::{Deserialize, Serialize};

/// The operator's two choices for one session. `effort: None` and
/// `fast: false` mean *the provider's own default* — eave passes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tuning {
    /// The CLI's own word for the level, verbatim (`xhigh`, not "Extra
    /// high"). `None` = do not pass anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The provider's fast lane. `false` = do not pass anything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    /// The model this session runs under NOW, the CLI's own id verbatim
    /// (Slice TN2). `None` = whatever the session was opened with
    /// (`AgentConfig::model`, or the CLI's default when that is `None`
    /// too). The engine resolves that into the effective model before it
    /// compares or applies anything, so an adapter never sees a `None`
    /// that means "unknown".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Tuning {
    pub fn is_default(&self) -> bool {
        self.effort.is_none() && !self.fast && self.model.is_none()
    }

    /// `self` with the model filled in from `opened_with` when it names
    /// none — the tuning a process actually runs under.
    pub fn effective(&self, opened_with: Option<&str>) -> Tuning {
        Tuning {
            model: self
                .model
                .clone()
                .or_else(|| opened_with.map(str::to_string)),
            ..self.clone()
        }
    }
}

/// When a change reaches the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    /// The running process takes the new value for its next turn.
    NextTurn,
    /// Only argv carries it: the agent is restarted between turns and the
    /// conversation resumed under the provider's cursor.
    NextSpawn,
}

/// Whether, and how, this provider's CLI takes an effort level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSupport {
    /// The CLI takes a level. `levels` are ITS words, lowest first, as
    /// measured — never eave's; `evidence` says where they were read.
    Levels {
        levels: &'static [&'static str],
        applies: Applies,
        evidence: &'static str,
    },
    /// Not offered, and why. Shown verbatim when a caller asks anyway.
    None { reason: &'static str },
}

/// Whether, and how, this provider's CLI has a fast lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedSupport {
    Toggle {
        applies: Applies,
        evidence: &'static str,
    },
    None {
        reason: &'static str,
    },
}

/// Whether, and how, this provider's CLI switches model on a running
/// conversation (Slice TN2). eave does not validate the id — that is
/// the CLI's error to state, the same rule `AgentConfig::model` follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSupport {
    Switch {
        applies: Applies,
        evidence: &'static str,
    },
    None {
        reason: &'static str,
    },
}

/// One adapter's whole answer. A required associated const on
/// [`crate::provider::AgentAdapter`], like [`crate::provider::RewindSupport`]:
/// no default, no third state, an adapter that has not answered does not
/// compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuningSupport {
    pub effort: EffortSupport,
    pub speed: SpeedSupport,
    pub model: ModelSupport,
}

impl TuningSupport {
    pub const NONE_UNMEASURED: TuningSupport = TuningSupport {
        effort: EffortSupport::None {
            reason: "not measured",
        },
        speed: SpeedSupport::None {
            reason: "not measured",
        },
        model: ModelSupport::None {
            reason: "not measured",
        },
    };

    pub fn levels(&self) -> &'static [&'static str] {
        match self.effort {
            EffortSupport::Levels { levels, .. } => levels,
            EffortSupport::None { .. } => &[],
        }
    }

    pub fn has_effort(&self) -> bool {
        matches!(self.effort, EffortSupport::Levels { .. })
    }

    pub fn has_speed(&self) -> bool {
        matches!(self.speed, SpeedSupport::Toggle { .. })
    }

    /// Can this CLI change model on a running conversation?
    pub fn has_model(&self) -> bool {
        matches!(self.model, ModelSupport::Switch { .. })
    }

    pub fn model_refusal(&self) -> Option<&'static str> {
        match self.model {
            ModelSupport::None { reason } => Some(reason),
            _ => None,
        }
    }

    /// Why the effort knob is absent, when it is.
    pub fn effort_refusal(&self) -> Option<&'static str> {
        match self.effort {
            EffortSupport::None { reason } => Some(reason),
            _ => None,
        }
    }

    pub fn speed_refusal(&self) -> Option<&'static str> {
        match self.speed {
            SpeedSupport::None { reason } => Some(reason),
            _ => None,
        }
    }

    /// Refuse a tuning this provider cannot carry, with the reason a
    /// person can read. `Ok` says nothing about WHEN it takes hold — see
    /// [`Self::applies`].
    pub fn check(&self, provider_id: &str, t: &Tuning) -> Result<(), String> {
        if let Some(level) = t.effort.as_deref() {
            match self.effort {
                EffortSupport::None { reason } => {
                    return Err(format!("{provider_id} takes no effort level: {reason}"));
                }
                EffortSupport::Levels { levels, .. } => {
                    if !levels.contains(&level) {
                        return Err(format!(
                            "{provider_id} has no effort level {level:?}; it takes: {}",
                            levels.join(", ")
                        ));
                    }
                }
            }
        }
        if t.fast {
            if let SpeedSupport::None { reason } = self.speed {
                return Err(format!("{provider_id} has no fast lane: {reason}"));
            }
        }
        if t.model.is_some() {
            if let ModelSupport::None { reason } = self.model {
                return Err(format!(
                    "{provider_id} cannot switch model on a running conversation: {reason}"
                ));
            }
        }
        Ok(())
    }

    /// The slowest way any axis of `t` (or of a change away from it)
    /// reaches the model. `None` when no supported axis is involved.
    pub fn applies(&self) -> Option<Applies> {
        let mut out = None;
        if let EffortSupport::Levels { applies, .. } = self.effort {
            out = Some(applies);
        }
        if let SpeedSupport::Toggle { applies, .. } = self.speed {
            out = Some(match (out, applies) {
                (Some(Applies::NextSpawn), _) | (_, Applies::NextSpawn) => Applies::NextSpawn,
                _ => Applies::NextTurn,
            });
        }
        if let ModelSupport::Switch { applies, .. } = self.model {
            out = Some(match (out, applies) {
                (Some(Applies::NextSpawn), _) | (_, Applies::NextSpawn) => Applies::NextSpawn,
                _ => Applies::NextTurn,
            });
        }
        out
    }

    /// Does going from `have` to `want` need a restart on this provider?
    pub fn needs_respawn(&self, have: &Tuning, want: &Tuning) -> bool {
        let effort_moved = have.effort != want.effort
            && matches!(
                self.effort,
                EffortSupport::Levels {
                    applies: Applies::NextSpawn,
                    ..
                }
            );
        let speed_moved = have.fast != want.fast
            && matches!(
                self.speed,
                SpeedSupport::Toggle {
                    applies: Applies::NextSpawn,
                    ..
                }
            );
        let model_moved = have.model != want.model
            && matches!(
                self.model,
                ModelSupport::Switch {
                    applies: Applies::NextSpawn,
                    ..
                }
            );
        effort_moved || speed_moved || model_moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPPORT: TuningSupport = TuningSupport {
        effort: EffortSupport::Levels {
            levels: &["low", "high"],
            applies: Applies::NextSpawn,
            evidence: "test",
        },
        speed: SpeedSupport::Toggle {
            applies: Applies::NextTurn,
            evidence: "test",
        },
        model: ModelSupport::Switch {
            applies: Applies::NextTurn,
            evidence: "test",
        },
    };

    /// Slice TN2. A model is the third axis: refused where the CLI cannot
    /// switch, resolved against what the session was opened with, and a
    /// live axis here so a switch alone never restarts anything.
    #[test]
    fn a_model_is_an_axis_resolved_against_what_the_session_opened_with() {
        let want = Tuning {
            model: Some("opus".into()),
            ..Tuning::default()
        };
        assert!(SUPPORT.check("x", &want).is_ok());
        let err = TuningSupport::NONE_UNMEASURED
            .check("x", &want)
            .unwrap_err();
        assert!(err.contains("cannot switch model"), "{err}");
        assert!(
            !SUPPORT.needs_respawn(&Tuning::default(), &want),
            "live here"
        );

        let unset = Tuning::default();
        assert_eq!(unset.effective(Some("sonnet")).model.as_deref(), Some("sonnet"));
        assert_eq!(want.effective(Some("sonnet")).model.as_deref(), Some("opus"));
        assert_eq!(unset.effective(None).model, None);
        assert!(!want.is_default());
    }

    #[test]
    fn a_level_the_cli_never_named_is_refused_with_the_list() {
        let t = Tuning {
            model: None,
            effort: Some("ultra".into()),
            fast: false,
        };
        let err = SUPPORT.check("x", &t).unwrap_err();
        assert!(
            err.contains("\"ultra\"") && err.contains("low, high"),
            "{err}"
        );
        assert!(SUPPORT
            .check(
                "x",
                &Tuning {
                    model: None,
                    effort: Some("high".into()),
                    fast: true
                }
            )
            .is_ok());
    }

    #[test]
    fn an_absent_axis_refuses_with_its_reason() {
        let none = TuningSupport::NONE_UNMEASURED;
        assert!(none.check("x", &Tuning::default()).is_ok());
        let err = none
            .check(
                "x",
                &Tuning {
                    model: None,
                    effort: None,
                    fast: true,
                },
            )
            .unwrap_err();
        assert!(
            err.contains("no fast lane") && err.contains("not measured"),
            "{err}"
        );
    }

    #[test]
    fn a_respawn_is_needed_only_for_an_argv_axis_that_moved() {
        let a = Tuning {
            model: None,
            effort: Some("low".into()),
            fast: false,
        };
        let b = Tuning {
            model: None,
            effort: Some("low".into()),
            fast: true,
        };
        assert!(!SUPPORT.needs_respawn(&a, &b), "speed is next-turn here");
        let c = Tuning {
            model: None,
            effort: Some("high".into()),
            fast: true,
        };
        assert!(SUPPORT.needs_respawn(&b, &c), "effort is argv here");
        assert_eq!(SUPPORT.applies(), Some(Applies::NextSpawn));
        assert_eq!(TuningSupport::NONE_UNMEASURED.applies(), None);
    }

    #[test]
    fn the_default_tuning_serialises_to_nothing() {
        let raw = serde_json::to_string(&Tuning::default()).unwrap();
        assert_eq!(raw, "{}");
        let back: Tuning = serde_json::from_str("{}").unwrap();
        assert!(back.is_default());
    }
}
