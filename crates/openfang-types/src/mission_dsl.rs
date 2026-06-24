//! Mission DSL — a pragmatic, auditable subset of the ABMS planning ontology
//! described in `tactical-assets/agents/mc/promt.md`.
//!
//! The DSL is the authoritative, typed product of compiling a natural-language
//! commander intent. It sits between the *structured intent* (what the operator
//! asked for) and the *fast-loop platform commands* (how a single platform
//! actually executes), preserving the traceable chain
//! `Mission → Objectives/Constraints → Plays → Functions` so that:
//!
//! - the LLM never directly produces a lethal command — Plays/Functions are
//!   deterministic templates the compiler binds,
//! - every product is human-auditable (typed structure is authoritative, the
//!   [`std::fmt::Display`] rendering is the operator-facing approval text),
//! - lethal actions always carry an intervention point and a safety guard.
//!
//! This is intentionally a *subset*: full multi-objective optimization,
//! digital-twin shadow execution and adversarial Monte-Carlo robustness from
//! `promt.md` are out of scope for the single-platform autonomous phase and are
//! reserved for later work.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::platform::{CcaRole, PlatformCommand, TurnDirection, Waypoint};
use crate::tactical::CommandClass;
use crate::umaa::WeaponReleaseLevel;

/// Top-level mission classes the compiler can emit. `Unknown` is a safe
/// fall-through used when intent extraction cannot confidently classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MissionKind {
    /// Direct engagement of a designated/known track.
    Engage,
    /// Flank-and-strike: recon approaches from behind, then a coordinated hit.
    ReconFlankStrike,
    /// Time-on-target coordinated strike across platforms.
    CoordinatedStrike,
    /// Reconnaissance / ISR collection only.
    Recon,
    /// Routine area patrol.
    Patrol,
    /// Return to base / recovery.
    Rtb,
    /// Track-only (no weapon release).
    Track,
    /// Close-in hard-kill self-defense / counter-swarm (autocannon CIWS).
    PointDefense,
    /// Over-the-horizon targeting / midcourse guidance handoff to a shooter.
    TargetingHandoff,
    /// Forward screening / early-warning picket station.
    Picket,
    /// Escort / protective accompaniment of a high-value unit.
    Escort,
    /// Maritime interdiction: stop, query and deny passage to a contact.
    MaritimeInterdiction,
    /// Non-kinetic deception / decoy / feint to shape adversary behavior.
    Deception,
    /// Direct sensor control requested by an operator or autonomy service.
    SensorControl,
    /// Reactive self-defense plan: evade, soft-kill and launch ISR support.
    ReactiveDefense,
    /// Could not be confidently classified.
    #[default]
    Unknown,
}

impl MissionKind {
    /// Human-readable label used in the rendered approval text.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Engage => "Engage",
            Self::ReconFlankStrike => "ReconFlankStrike",
            Self::CoordinatedStrike => "CoordinatedStrike",
            Self::Recon => "Recon",
            Self::Patrol => "Patrol",
            Self::Rtb => "Rtb",
            Self::Track => "Track",
            Self::PointDefense => "PointDefense",
            Self::TargetingHandoff => "TargetingHandoff",
            Self::Picket => "Picket",
            Self::Escort => "Escort",
            Self::MaritimeInterdiction => "MaritimeInterdiction",
            Self::Deception => "Deception",
            Self::SensorControl => "SensorControl",
            Self::ReactiveDefense => "ReactiveDefense",
            Self::Unknown => "Unknown",
        }
    }

    /// Whether this mission class is expected to involve weapon release. Used by
    /// the validator (R6) to require an approval intervention point.
    pub fn is_lethal_class(&self) -> bool {
        matches!(
            self,
            Self::Engage
                | Self::ReconFlankStrike
                | Self::CoordinatedStrike
                // Point defense (CIWS) and maritime interdiction (warning/disabling
                // fire) both may employ the autocannon, so they require the same
                // approval intervention point as the offensive strike classes.
                | Self::PointDefense
                | Self::MaritimeInterdiction
        )
    }
}

/// Service lane that should own a mission function at the DSL/audit level.
///
/// This mirrors the runtime cerebellum service labels without making
/// `openfang-types` depend on `openfang-runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionService {
    Sms,
    Mms,
    Wms,
    Spgs,
    Acs,
    Ewms,
    Cms,
    Pss,
}

impl MissionService {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Mms => "mms",
            Self::Wms => "wms",
            Self::Spgs => "spgs",
            Self::Acs => "acs",
            Self::Ewms => "ewms",
            Self::Cms => "cms",
            Self::Pss => "pss",
        }
    }

    pub fn from_command_class(class: CommandClass) -> Self {
        match class {
            CommandClass::Motion | CommandClass::Formation | CommandClass::Uav => Self::Mms,
            CommandClass::Sensor => Self::Sms,
            CommandClass::Weapon => Self::Wms,
            CommandClass::ElectronicWarfare => Self::Ewms,
            CommandClass::Comm => Self::Cms,
            CommandClass::Aux => Self::Pss,
            CommandClass::Command => Self::Acs,
        }
    }
}

/// A simplified Objective+KR. The `feedback_var` is the closed-loop reference
/// variable (promt.md §11 R3) the cognition assessment can re-evaluate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DslObjective {
    pub id: String,
    pub description: String,
    /// Closed-loop feedback variable name (e.g. `track:trk-1:engaged`,
    /// `isr_coverage`, `standoff_m`). Required for R3.
    #[serde(default)]
    pub feedback_var: Option<String>,
    pub priority: u32,
}

/// What a [`Constraint`] governs. Hard constraints must hold; soft constraints
/// are advisory/penalized.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConstraintKind {
    /// Minimum safe distance to the target/threat, in meters.
    Standoff { meters: f64 },
    /// Rules-of-engagement weapon release ceiling.
    Roe { level: WeaponReleaseLevel },
    /// Positive identification required before any kinetic action.
    PidRequired,
    /// Mission must complete within this many seconds of issue.
    Deadline { seconds: f64 },
}

/// A verifiable check attached to a constraint (promt.md §3 `Check`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub method: String,
    pub threshold: f64,
    #[serde(default)]
    pub window_s: Option<f64>,
}

/// A hard/soft constraint with an optional online check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraint {
    pub kind: ConstraintKind,
    pub hard: bool,
    #[serde(default)]
    pub check: Option<Check>,
}

impl Constraint {
    pub fn standoff(meters: f64, hard: bool) -> Self {
        Self {
            kind: ConstraintKind::Standoff { meters },
            hard,
            check: Some(Check {
                method: "cpa_distance_m".into(),
                threshold: meters,
                window_s: None,
            }),
        }
    }

    pub fn roe(level: WeaponReleaseLevel) -> Self {
        Self {
            kind: ConstraintKind::Roe { level },
            hard: true,
            check: None,
        }
    }

    pub fn pid_required() -> Self {
        Self {
            kind: ConstraintKind::PidRequired,
            hard: true,
            check: Some(Check {
                method: "positive_identification".into(),
                threshold: 1.0,
                window_s: None,
            }),
        }
    }

    pub fn deadline(seconds: f64) -> Self {
        Self {
            kind: ConstraintKind::Deadline { seconds },
            hard: false,
            check: Some(Check {
                method: "elapsed_s".into(),
                threshold: seconds,
                window_s: None,
            }),
        }
    }

    /// Standoff distance in meters, if this is a standoff constraint.
    pub fn standoff_m(&self) -> Option<f64> {
        match self.kind {
            ConstraintKind::Standoff { meters } => Some(meters),
            _ => None,
        }
    }
}

/// An instance of a tactical template (Play) selected for this mission, with
/// the platforms and role bound to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayInstance {
    pub play_id: String,
    pub assigned_platforms: Vec<String>,
    pub role: CcaRole,
    /// Ordering hint for single-platform serialization (lower runs first).
    #[serde(default)]
    pub phase: u32,
}

/// The atomic action specification produced by a Function. Kept higher-level
/// than [`PlatformCommand`] so it is platform-id-agnostic until bound, and so
/// the lethal subset is explicit for the validator and gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlatformCommandSpec {
    /// Fly a route (used for flank approach + patrol legs).
    FollowRoute { waypoints: Vec<Waypoint> },
    /// Direct navigation to a point (RTB / goto).
    Goto {
        lat: f64,
        lon: f64,
        #[serde(default)]
        alt: Option<f64>,
        #[serde(default)]
        speed_ms: Option<f64>,
    },
    /// Turn to an absolute heading (optionally with cruise speed and turn side).
    SetHeading {
        heading_deg: f64,
        #[serde(default)]
        speed_ms: Option<f64>,
        #[serde(default)]
        turn_direction: Option<TurnDirection>,
    },
    /// Set platform speed (m/s).
    SetSpeed { speed_ms: f64 },
    /// Turn a sensor on.
    SensorOn { sensor_id: String },
    /// Turn a sensor off.
    SensorOff { sensor_id: String },
    /// Set a sensor mode (e.g. "track", "search").
    SensorSetMode { sensor_id: String, mode: String },
    /// Designate / lock a track for shooters.
    Designate { track_id: String },
    /// Kinetic: fire a weapon at a track (lethal).
    Fire {
        weapon_id: String,
        track_id: String,
        #[serde(default)]
        salvo_size: Option<u32>,
    },
    /// Kinetic: time-on-target coordinated strike (lethal).
    CoordinatedStrike {
        strike_platform_ids: Vec<String>,
        target_id: String,
        time_on_target_us: u64,
    },
    /// Soft-kill: start active jamming.
    Jam {
        jammer_id: String,
        #[serde(default)]
        technique: Option<String>,
        #[serde(default)]
        frequency_hz: Option<f64>,
        #[serde(default)]
        bandwidth_hz: Option<f64>,
        #[serde(default)]
        target_track_id: Option<String>,
    },
    /// Soft-kill: stop active jamming.
    JamStop { jammer_id: String },
    /// CMS: send a message to another platform through ArkSIM comms.
    SendMessage {
        to_platform_id: String,
        message: String,
    },
    /// Soft-kill: release chaff / decoy countermeasures.
    ReleaseDecoy {
        weapon_id: String,
        #[serde(default = "default_decoy_count")]
        count: u32,
        #[serde(default = "default_decoy_interval_s")]
        interval_s: f64,
    },
    /// Launch a carried or controlled UAV.
    LaunchUav { uav_id: String },
    /// Make all weapons safe.
    WeaponSafe,
}

fn default_decoy_count() -> u32 {
    1
}

fn default_decoy_interval_s() -> f64 {
    0.25
}

/// Recognise a reconnaissance-UAV slot by its weapon id. Such "weapons" deploy
/// an ISR drone (lowered to `FireAtTarget` on the wire) rather than releasing a
/// kinetic munition, so they are exempt from lethal release-geometry / standoff
/// gating and weapon-release approval. The id naming follows the platform
/// component config (e.g. `scout_uav_slot`).
pub fn is_recon_uav_weapon_id(weapon_id: &str) -> bool {
    let id = weapon_id.to_ascii_lowercase();
    id.contains("scout_uav")
        || id.contains("scout uav")
        || id.contains("recon_uav")
        || id.contains("j7_uav")
        || id.contains("uav_weapon")
}

impl PlatformCommandSpec {
    /// Whether executing this spec releases a weapon. Drives R6 validation,
    /// the standoff gate, and the weapon/mission-approval interlocks.
    ///
    /// A reconnaissance-UAV slot release is **not** lethal: the scout UAV is an
    /// ISR asset that happens to be deployed through the `FireAtTarget` wire
    /// command, so it lowers like a fire but must bypass lethal release-geometry
    /// / standoff gating (you launch a scout toward a bearing precisely to *find*
    /// the target, even when its exact pose is not yet pinned).
    pub fn is_lethal(&self) -> bool {
        match self {
            Self::Fire { weapon_id, .. } => !is_recon_uav_weapon_id(weapon_id),
            Self::CoordinatedStrike { .. } => true,
            _ => false,
        }
    }

    /// True when this spec deploys a reconnaissance UAV (ISR asset) rather than
    /// releasing a kinetic munition. Such releases lower to `FireAtTarget` on the
    /// wire but are tactically a recon action, not a weapon employment.
    pub fn is_isr_release(&self) -> bool {
        matches!(self, Self::Fire { weapon_id, .. } if is_recon_uav_weapon_id(weapon_id))
    }

    /// Coarse command class used for service routing and mutex checks.
    pub fn command_class(&self) -> CommandClass {
        match self {
            Self::FollowRoute { .. }
            | Self::Goto { .. }
            | Self::SetHeading { .. }
            | Self::SetSpeed { .. } => CommandClass::Motion,
            Self::SensorOn { .. } | Self::SensorOff { .. } | Self::SensorSetMode { .. } => {
                CommandClass::Sensor
            }
            Self::Designate { .. }
            | Self::Fire { .. }
            | Self::CoordinatedStrike { .. }
            | Self::WeaponSafe => CommandClass::Weapon,
            Self::Jam { .. } | Self::JamStop { .. } | Self::ReleaseDecoy { .. } => {
                CommandClass::ElectronicWarfare
            }
            Self::SendMessage { .. } => CommandClass::Comm,
            Self::LaunchUav { .. } => CommandClass::Uav,
        }
    }

    pub fn default_service(&self) -> MissionService {
        MissionService::from_command_class(self.command_class())
    }

    /// Lower this spec into a concrete [`PlatformCommand`] bound to `platform_id`.
    /// For a coordinated strike, `platform_id` is the coordinator.
    pub fn to_platform_command(&self, platform_id: &str) -> PlatformCommand {
        match self {
            Self::FollowRoute { waypoints } => PlatformCommand::FollowRoute {
                platform_id: platform_id.to_string(),
                waypoints: waypoints.clone(),
            },
            Self::Goto {
                lat,
                lon,
                alt,
                speed_ms,
            } => PlatformCommand::GotoLocation {
                platform_id: platform_id.to_string(),
                lat: *lat,
                lon: *lon,
                alt: *alt,
                speed_ms: *speed_ms,
            },
            Self::SetHeading {
                heading_deg,
                speed_ms,
                turn_direction,
            } => PlatformCommand::SetHeading {
                platform_id: platform_id.to_string(),
                heading_deg: *heading_deg,
                speed_ms: *speed_ms,
                turn_direction: *turn_direction,
            },
            Self::SetSpeed { speed_ms } => PlatformCommand::SetSpeed {
                platform_id: platform_id.to_string(),
                speed_ms: *speed_ms,
                acceleration_ms2: None,
            },
            Self::SensorOn { sensor_id } => PlatformCommand::SensorOn {
                platform_id: platform_id.to_string(),
                sensor_id: sensor_id.clone(),
            },
            Self::SensorOff { sensor_id } => PlatformCommand::SensorOff {
                platform_id: platform_id.to_string(),
                sensor_id: sensor_id.clone(),
            },
            Self::SensorSetMode { sensor_id, mode } => PlatformCommand::SensorSetMode {
                platform_id: platform_id.to_string(),
                sensor_id: sensor_id.clone(),
                mode: mode.clone(),
            },
            Self::Designate { track_id } => PlatformCommand::UpdateTarget {
                platform_id: platform_id.to_string(),
                track_id: track_id.clone(),
            },
            Self::Fire {
                weapon_id,
                track_id,
                salvo_size,
            } => match salvo_size.filter(|size| *size > 1) {
                Some(size) => PlatformCommand::FireSalvo {
                    platform_id: platform_id.to_string(),
                    weapon_id: weapon_id.clone(),
                    track_id: track_id.clone(),
                    salvo_size: size,
                },
                None => PlatformCommand::FireAtTarget {
                    platform_id: platform_id.to_string(),
                    weapon_id: weapon_id.clone(),
                    track_id: track_id.clone(),
                },
            },
            Self::CoordinatedStrike {
                strike_platform_ids,
                target_id,
                time_on_target_us,
            } => PlatformCommand::CoordinatedStrike {
                coordinator_platform_id: platform_id.to_string(),
                strike_platform_ids: strike_platform_ids.clone(),
                target_id: target_id.clone(),
                time_on_target_us: *time_on_target_us,
            },
            Self::Jam {
                jammer_id,
                frequency_hz,
                bandwidth_hz,
                target_track_id,
                ..
            } => PlatformCommand::JamStart {
                platform_id: platform_id.to_string(),
                jammer_id: jammer_id.clone(),
                frequency_hz: frequency_hz.unwrap_or(0.0),
                bandwidth_hz: bandwidth_hz.unwrap_or(0.0),
                target_track_id: target_track_id.clone().unwrap_or_default(),
            },
            Self::JamStop { jammer_id } => PlatformCommand::JamStop {
                platform_id: platform_id.to_string(),
                jammer_id: jammer_id.clone(),
            },
            Self::SendMessage {
                to_platform_id,
                message,
            } => PlatformCommand::SendMessage {
                from_platform_id: platform_id.to_string(),
                to_platform_id: to_platform_id.clone(),
                message: message.clone(),
            },
            Self::ReleaseDecoy {
                weapon_id,
                count,
                interval_s,
            } => PlatformCommand::FireChaff {
                platform_id: platform_id.to_string(),
                weapon_id: weapon_id.clone(),
                count: *count,
                interval_s: *interval_s,
            },
            Self::LaunchUav { uav_id } => PlatformCommand::LaunchUav {
                uav_id: uav_id.clone(),
            },
            Self::WeaponSafe => PlatformCommand::WeaponSafeAll {
                platform_id: platform_id.to_string(),
            },
        }
    }

    /// Short label for the rendered approval text.
    pub fn label(&self) -> &'static str {
        match self {
            Self::FollowRoute { .. } => "FollowRoute",
            Self::Goto { .. } => "Goto",
            Self::SetHeading { .. } => "SetHeading",
            Self::SetSpeed { .. } => "SetSpeed",
            Self::SensorOn { .. } => "SensorOn",
            Self::SensorOff { .. } => "SensorOff",
            Self::SensorSetMode { .. } => "SensorSetMode",
            Self::Designate { .. } => "Designate",
            Self::Fire { .. } => "Fire",
            Self::CoordinatedStrike { .. } => "CoordinatedStrike",
            Self::Jam { .. } => "Jam",
            Self::JamStop { .. } => "JamStop",
            Self::SendMessage { .. } => "SendMessage",
            Self::ReleaseDecoy { .. } => "ReleaseDecoy",
            Self::LaunchUav { .. } => "LaunchUav",
            Self::WeaponSafe => "WeaponSafe",
        }
    }
}

/// Safety boundary attached to a Function (promt.md §5 `SafetyGuard`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SafetyGuard {
    pub preconditions: Vec<String>,
    pub abort_rules: Vec<String>,
    pub lme_checklist: Vec<String>,
}

/// An atomic action bound to a platform, produced from a Play's function list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub id: String,
    /// Stable symbolic task id used by NLP-generated dependency graphs.
    #[serde(default)]
    pub task_id: String,
    /// `play_id` of the [`PlayInstance`] this function belongs to (R4 traceability).
    pub parent_play: String,
    pub platform_id: String,
    pub command: PlatformCommandSpec,
    /// Preconditions such as `T1_complete`, `event:missile_inbound`, or
    /// `feedback:track:trk-1:engaged==1`.
    #[serde(default)]
    pub preconditions: Vec<String>,
    /// Completion / inspection criterion supplied by the symbolic task plan.
    #[serde(default)]
    pub criteria: Option<String>,
    /// Audit-only phase emitted by the LLM task graph. Runtime ordering is
    /// derived from preconditions; this only preserves the operator-facing plan.
    #[serde(default)]
    pub phase: u32,
    /// Audit-only ordering within a phase.
    #[serde(default)]
    pub ordering: u32,
    /// Optional explicit service override; defaults from the command class.
    #[serde(default)]
    pub service: Option<MissionService>,
    #[serde(default)]
    pub safety_guard: SafetyGuard,
}

impl FunctionCall {
    pub fn task_ref(&self) -> &str {
        if self.task_id.trim().is_empty() {
            &self.id
        } else {
            &self.task_id
        }
    }

    pub fn service(&self) -> MissionService {
        self.service
            .unwrap_or_else(|| self.command.default_service())
    }

    pub fn is_lethal(&self) -> bool {
        self.command.is_lethal()
    }
}

/// Human-intervention level (promt.md §4 / constraint 5), Level-0..Level-4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionLevel {
    /// Level-0: observe only.
    Monitor,
    /// Level-1: tune weights.
    Tuning,
    /// Level-2: approve/reject Plays.
    ApproveReject,
    /// Level-3: override Functions.
    Override,
    /// Level-4: safe-halt.
    SafeHalt,
}

/// Action requested when an intervention point fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionAction {
    /// Require explicit human approval before proceeding.
    RequireApproval,
    /// Allow a human to redirect the plan.
    AllowHumanRedirect,
    /// Escalate to a human operator.
    EscalateToHuman,
    /// Bring the platform to a safe halt.
    SafeHalt,
    /// Notify only (non-blocking).
    Notify,
}

/// A declared intervention anchor (promt.md constraint 5 mandatory field).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionPoint {
    /// Trigger condition, free text (e.g. `before_weapon_release`).
    pub on: String,
    pub level: InterventionLevel,
    pub action: InterventionAction,
}

impl InterventionPoint {
    /// The mandatory pre-fire approval anchor for lethal functions (R6).
    pub fn require_approval_before_fire() -> Self {
        Self {
            on: "before_weapon_release".into(),
            level: InterventionLevel::ApproveReject,
            action: InterventionAction::RequireApproval,
        }
    }
}

/// A compiled, auditable mission. The typed structure is authoritative; the
/// [`std::fmt::Display`] rendering is the operator-facing approval text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDsl {
    /// `hash(intent, time, theater)` style stable id (promt.md §1).
    pub id: String,
    /// Original natural-language intent text.
    pub intent_text: String,
    pub kind: MissionKind,
    /// Optional `[start, end]` mission window in seconds (epoch/sim).
    #[serde(default)]
    pub time_window: Option<(f64, f64)>,
    pub objectives: Vec<DslObjective>,
    pub constraints: Vec<Constraint>,
    pub plays: Vec<PlayInstance>,
    pub functions: Vec<FunctionCall>,
    pub intervention_points: Vec<InterventionPoint>,
    /// `M→Play→Func` mapping chain, human readable (promt.md `explanation_trace`).
    pub explanation_trace: String,
    /// Extraction/compilation confidence in `[0, 1]`.
    pub confidence: f64,
    /// Where this mission came from (promt.md `provenance`).
    pub provenance: String,
}

impl MissionDsl {
    /// Standoff distance in meters declared by the first standoff constraint.
    pub fn standoff_m(&self) -> Option<f64> {
        self.constraints.iter().find_map(Constraint::standoff_m)
    }

    /// The ROE ceiling declared by the first ROE constraint, if any.
    pub fn roe(&self) -> Option<WeaponReleaseLevel> {
        self.constraints.iter().find_map(|c| match c.kind {
            ConstraintKind::Roe { level } => Some(level),
            _ => None,
        })
    }

    /// Whether any function in this mission releases a weapon.
    pub fn has_lethal_function(&self) -> bool {
        self.functions.iter().any(FunctionCall::is_lethal)
    }

    /// Run the mapping-correctness validator (promt.md §11 subset).
    pub fn validate(&self) -> Vec<ValidationIssue> {
        validate_mission(self)
    }

    /// Convenience: no validation issues.
    pub fn is_valid(&self) -> bool {
        self.validate().is_empty()
    }
}

// ─────────────────────────────────────────────
// Mapping-correctness validator (promt.md §11 subset: R1/R3/R4/R6/R7)
// ─────────────────────────────────────────────

/// Which mapping rule a [`ValidationIssue`] relates to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationRule {
    /// Intent coverage: spatial/temporal/cost elements have Objective/Constraint.
    R1,
    /// Closed-loop: each objective has a feedback variable.
    R3,
    /// Layering: high-level kind never produces a Function without a parent Play.
    R4,
    /// Human-in-the-loop: lethal functions carry an approval intervention point.
    R6,
    /// Conflict resolution: no duplicate/conflicting platform-resource grabs.
    R7,
}

/// A single mapping-correctness finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub rule: ValidationRule,
    pub message: String,
}

fn validate_mission(mission: &MissionDsl) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    // R1 — intent coverage.
    if mission.objectives.is_empty() {
        issues.push(ValidationIssue {
            rule: ValidationRule::R1,
            message: "mission has no objectives covering the intent".into(),
        });
    }
    if mission.time_window.is_some()
        && !mission
            .constraints
            .iter()
            .any(|c| matches!(c.kind, ConstraintKind::Deadline { .. }))
    {
        issues.push(ValidationIssue {
            rule: ValidationRule::R1,
            message: "intent specifies a time window but no Deadline constraint exists".into(),
        });
    }
    if mission.kind.is_lethal_class() {
        if mission.plays.is_empty() {
            issues.push(ValidationIssue {
                rule: ValidationRule::R1,
                message: format!(
                    "high-risk mission '{}' selected no tactical plays; target or capability grounding likely failed",
                    mission.kind.label()
                ),
            });
        }
        if mission.functions.is_empty() {
            issues.push(ValidationIssue {
                rule: ValidationRule::R1,
                message: format!(
                    "high-risk mission '{}' selected no executable functions; mission must be clarified before dispatch",
                    mission.kind.label()
                ),
            });
        }
        if !mission.functions.iter().any(FunctionCall::is_lethal) {
            issues.push(ValidationIssue {
                rule: ValidationRule::R1,
                message: format!(
                    "high-risk mission '{}' contains no lethal function after compilation",
                    mission.kind.label()
                ),
            });
        }
    }

    // R3 — closed-loop feedback variable per objective.
    for objective in &mission.objectives {
        if objective
            .feedback_var
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            issues.push(ValidationIssue {
                rule: ValidationRule::R3,
                message: format!(
                    "objective '{}' has no closed-loop feedback variable",
                    objective.id
                ),
            });
        }
    }

    // R4 — layering: every function must trace to an existing play.
    for function in &mission.functions {
        let parent_ok = !function.parent_play.trim().is_empty()
            && mission
                .plays
                .iter()
                .any(|play| play.play_id == function.parent_play);
        if !parent_ok {
            issues.push(ValidationIssue {
                rule: ValidationRule::R4,
                message: format!(
                    "function '{}' does not trace to a Play (parent_play='{}')",
                    function.id, function.parent_play
                ),
            });
        }
    }

    // R6 — human-in-the-loop for lethal functions.
    if mission.has_lethal_function() {
        let has_approval = mission
            .intervention_points
            .iter()
            .any(|point| matches!(point.action, InterventionAction::RequireApproval));
        if !has_approval {
            issues.push(ValidationIssue {
                rule: ValidationRule::R6,
                message: "lethal function present but no require_approval intervention point"
                    .into(),
            });
        }
        for function in mission.functions.iter().filter(|f| f.is_lethal()) {
            if function.safety_guard.preconditions.is_empty() {
                issues.push(ValidationIssue {
                    rule: ValidationRule::R6,
                    message: format!(
                        "lethal function '{}' has no safety-guard preconditions",
                        function.id
                    ),
                });
            }
        }
    }

    // R7 — conflict resolution: unique function ids and no weapon mutex conflict
    // (same platform firing at two different tracks in one mission).
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_tasks = std::collections::HashSet::new();
    for function in &mission.functions {
        if !seen_ids.insert(function.id.as_str()) {
            issues.push(ValidationIssue {
                rule: ValidationRule::R7,
                message: format!("duplicate function id '{}'", function.id),
            });
        }
        if !seen_tasks.insert(function.task_ref().to_string()) {
            issues.push(ValidationIssue {
                rule: ValidationRule::R7,
                message: format!("duplicate task id '{}'", function.task_ref()),
            });
        }
    }

    let task_ids: std::collections::HashSet<String> = mission
        .functions
        .iter()
        .map(|function| function.task_ref().to_string())
        .collect();
    for function in &mission.functions {
        for precondition in &function.preconditions {
            if let Some(task_id) = precondition.strip_suffix("_complete") {
                if !task_ids.contains(task_id) {
                    issues.push(ValidationIssue {
                        rule: ValidationRule::R7,
                        message: format!(
                            "function '{}' references unknown precondition '{}'",
                            function.task_ref(),
                            precondition
                        ),
                    });
                }
            }
        }
    }
    validate_task_graph_acyclic(mission, &mut issues);

    let mut weapon_targets: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for function in &mission.functions {
        if let PlatformCommandSpec::Fire { track_id, .. } = &function.command {
            if let Some(existing) = weapon_targets.insert(function.platform_id.as_str(), track_id) {
                if existing != track_id {
                    issues.push(ValidationIssue {
                        rule: ValidationRule::R7,
                        message: format!(
                            "platform '{}' allocated to conflicting fire targets '{}' and '{}'",
                            function.platform_id, existing, track_id
                        ),
                    });
                }
            }
        }
    }

    if mission
        .functions
        .iter()
        .any(|function| !function.preconditions.is_empty())
    {
        let mut lane_frontiers: std::collections::HashMap<String, &str> =
            std::collections::HashMap::new();
        for function in &mission.functions {
            let mut preconditions = function.preconditions.clone();
            preconditions.sort();
            let key = format!(
                "{}:{}:{:?}",
                function.platform_id,
                mutex_lane(&function.command),
                preconditions
            );
            if let Some(existing) = lane_frontiers.insert(key, function.task_ref()) {
                issues.push(ValidationIssue {
                    rule: ValidationRule::R7,
                    message: format!(
                        "tasks '{}' and '{}' may concurrently contend for platform '{}' {} lane",
                        existing,
                        function.task_ref(),
                        function.platform_id,
                        function.command.command_class().as_str()
                    ),
                });
            }
        }
    }

    issues
}

fn mutex_lane(command: &PlatformCommandSpec) -> String {
    match command {
        PlatformCommandSpec::SetHeading { .. } => "motion:heading".into(),
        PlatformCommandSpec::SetSpeed { .. } => "motion:speed".into(),
        PlatformCommandSpec::Goto { .. } => "motion:goto".into(),
        PlatformCommandSpec::FollowRoute { .. } => "motion:route".into(),
        PlatformCommandSpec::SensorOn { sensor_id }
        | PlatformCommandSpec::SensorOff { sensor_id }
        | PlatformCommandSpec::SensorSetMode { sensor_id, .. } => format!("sensor:{sensor_id}"),
        PlatformCommandSpec::Designate { .. } => "weapon:targeting".into(),
        PlatformCommandSpec::Fire { weapon_id, .. }
        | PlatformCommandSpec::ReleaseDecoy { weapon_id, .. } => format!("weapon:{weapon_id}"),
        PlatformCommandSpec::CoordinatedStrike { .. } => "weapon:coordinated_strike".into(),
        PlatformCommandSpec::Jam { jammer_id, .. } | PlatformCommandSpec::JamStop { jammer_id } => {
            format!("jam:{jammer_id}")
        }
        PlatformCommandSpec::SendMessage { to_platform_id, .. } => {
            format!("comm:message:{to_platform_id}")
        }
        PlatformCommandSpec::LaunchUav { uav_id } => format!("uav:{uav_id}"),
        PlatformCommandSpec::WeaponSafe => "weapon:safe".into(),
    }
}

fn validate_task_graph_acyclic(mission: &MissionDsl, issues: &mut Vec<ValidationIssue>) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Visit {
        Visiting,
        Done,
    }

    fn visit<'a>(
        id: &'a str,
        edges: &std::collections::HashMap<&'a str, Vec<&'a str>>,
        visits: &mut std::collections::HashMap<&'a str, Visit>,
    ) -> bool {
        if matches!(visits.get(id), Some(Visit::Visiting)) {
            return false;
        }
        if matches!(visits.get(id), Some(Visit::Done)) {
            return true;
        }
        visits.insert(id, Visit::Visiting);
        if let Some(deps) = edges.get(id) {
            for dep in deps {
                if !visit(dep, edges, visits) {
                    return false;
                }
            }
        }
        visits.insert(id, Visit::Done);
        true
    }

    let mut edges: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for function in &mission.functions {
        let deps = function
            .preconditions
            .iter()
            .filter_map(|precondition| precondition.strip_suffix("_complete"))
            .collect::<Vec<_>>();
        edges.insert(function.task_ref(), deps);
    }

    let mut visits = std::collections::HashMap::new();
    for function in &mission.functions {
        if !visit(function.task_ref(), &edges, &mut visits) {
            issues.push(ValidationIssue {
                rule: ValidationRule::R7,
                message: "task dependency graph contains a cycle".into(),
            });
            return;
        }
    }
}

// ─────────────────────────────────────────────
// Operator-facing rendering (promt.md MISSION{...} style)
// ─────────────────────────────────────────────

impl fmt::Display for MissionDsl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "MISSION {{")?;
        writeln!(f, "  id: \"{}\"", self.id)?;
        writeln!(f, "  type: {}", self.kind.label())?;
        writeln!(f, "  intent: \"{}\"", self.intent_text)?;
        if let Some((start, end)) = self.time_window {
            writeln!(f, "  time_window: [{start:.0}s, {end:.0}s]")?;
        }
        writeln!(f, "  confidence: {:.2}", self.confidence)?;

        writeln!(f, "  objectives: [")?;
        for objective in &self.objectives {
            let feedback = objective.feedback_var.as_deref().unwrap_or("-");
            writeln!(
                f,
                "    {{ id: \"{}\", desc: \"{}\", priority: {}, feedback: \"{}\" }}",
                objective.id, objective.description, objective.priority, feedback
            )?;
        }
        writeln!(f, "  ]")?;

        writeln!(f, "  constraints: [")?;
        for constraint in &self.constraints {
            let tag = if constraint.hard { "Hard" } else { "Soft" };
            writeln!(f, "    {tag}: {}", render_constraint(&constraint.kind))?;
        }
        writeln!(f, "  ]")?;

        writeln!(f, "  plays: [")?;
        for play in &self.plays {
            writeln!(
                f,
                "    {{ play: \"{}\", role: {:?}, phase: {}, platforms: [{}] }}",
                play.play_id,
                play.role,
                play.phase,
                play.assigned_platforms.join(", ")
            )?;
        }
        writeln!(f, "  ]")?;

        writeln!(f, "  functions: [")?;
        for function in &self.functions {
            let lethal = if function.is_lethal() {
                " (lethal)"
            } else {
                ""
            };
            let criteria = function.criteria.as_deref().unwrap_or("-");
            let preconditions = if function.preconditions.is_empty() {
                "-".into()
            } else {
                function.preconditions.join(", ")
            };
            writeln!(
                f,
                "    {{ task: \"{}\", id: \"{}\", play: \"{}\", platform: \"{}\", phase: {}, order: {}, service: {}, op: {}{}, pre: [{}], criteria: \"{}\" }}",
                function.task_ref(),
                function.id,
                function.parent_play,
                function.platform_id,
                function.phase,
                function.ordering,
                function.service().label(),
                function.command.label(),
                lethal,
                preconditions,
                criteria
            )?;
        }
        writeln!(f, "  ]")?;

        writeln!(f, "  intervention_points: [")?;
        for point in &self.intervention_points {
            writeln!(
                f,
                "    {{ on: \"{}\", level: {:?}, action: {:?} }}",
                point.on, point.level, point.action
            )?;
        }
        writeln!(f, "  ]")?;

        writeln!(f, "  explanation: \"{}\"", self.explanation_trace)?;
        writeln!(f, "  provenance: \"{}\"", self.provenance)?;
        write!(f, "}}")
    }
}

fn render_constraint(kind: &ConstraintKind) -> String {
    match kind {
        ConstraintKind::Standoff { meters } => format!("standoff_m >= {meters:.0}"),
        ConstraintKind::Roe { level } => format!("roe == {level:?}"),
        ConstraintKind::PidRequired => "pid_required == true".into(),
        ConstraintKind::Deadline { seconds } => format!("deadline_s <= {seconds:.0}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lethal_function(id: &str, play: &str, platform: &str, track: &str) -> FunctionCall {
        FunctionCall {
            id: id.into(),
            task_id: id.into(),
            parent_play: play.into(),
            platform_id: platform.into(),
            command: PlatformCommandSpec::Fire {
                weapon_id: "w1".into(),
                track_id: track.into(),
                salvo_size: None,
            },
            preconditions: Vec::new(),
            criteria: None,
            phase: 0,
            ordering: 0,
            service: None,
            safety_guard: SafetyGuard {
                preconditions: vec!["pid_confirmed".into()],
                abort_rules: vec!["target_lost".into()],
                lme_checklist: vec!["no_strike_list_clear".into()],
            },
        }
    }

    fn engage_mission() -> MissionDsl {
        MissionDsl {
            id: "mission:test".into(),
            intent_text: "engage hostile command post".into(),
            kind: MissionKind::Engage,
            time_window: None,
            objectives: vec![DslObjective {
                id: "obj-1".into(),
                description: "neutralize track trk-1".into(),
                feedback_var: Some("track:trk-1:engaged".into()),
                priority: 100,
            }],
            constraints: vec![Constraint::roe(WeaponReleaseLevel::WeaponsTight)],
            plays: vec![PlayInstance {
                play_id: "Engage".into(),
                assigned_platforms: vec!["self".into()],
                role: CcaRole::Striker,
                phase: 0,
            }],
            functions: vec![lethal_function("fn-fire", "Engage", "self", "trk-1")],
            intervention_points: vec![InterventionPoint::require_approval_before_fire()],
            explanation_trace: "M→Engage→Fire(trk-1)".into(),
            confidence: 0.9,
            provenance: "test".into(),
        }
    }

    #[test]
    fn lethal_spec_lowers_to_fire_command() {
        let spec = PlatformCommandSpec::Fire {
            weapon_id: "w1".into(),
            track_id: "trk-1".into(),
            salvo_size: None,
        };
        assert!(spec.is_lethal());
        assert!(matches!(
            spec.to_platform_command("self"),
            PlatformCommand::FireAtTarget { ref platform_id, .. } if platform_id == "self"
        ));
    }

    #[test]
    fn recon_uav_release_is_non_lethal_but_still_fires_on_the_wire() {
        let scout = PlatformCommandSpec::Fire {
            weapon_id: "scout_uav_slot".into(),
            track_id: "blue_command_post:1".into(),
            salvo_size: None,
        };
        // ISR deploy: not lethal, but still a FireAtTarget on the wire.
        assert!(!scout.is_lethal(), "recon UAV release must not be lethal");
        assert!(scout.is_isr_release());
        assert!(matches!(
            scout.to_platform_command("self"),
            PlatformCommand::FireAtTarget { ref weapon_id, .. } if weapon_id == "scout_uav_slot"
        ));

        // A kinetic munition on the same wire command stays lethal.
        let kinetic = PlatformCommandSpec::Fire {
            weapon_id: "loiter_wave1".into(),
            track_id: "blue_command_post:1".into(),
            salvo_size: None,
        };
        assert!(kinetic.is_lethal());
        assert!(!kinetic.is_isr_release());
    }

    #[test]
    fn j7_uav_weapon_release_is_non_lethal_isr() {
        let j7 = PlatformCommandSpec::Fire {
            weapon_id: "J7_UAV_WEAPON".into(),
            track_id: "blue_command_post:1".into(),
            salvo_size: None,
        };

        assert!(is_recon_uav_weapon_id("J7_UAV_WEAPON"));
        assert!(!j7.is_lethal(), "J7 UAV deploy must not be lethal");
        assert!(j7.is_isr_release());
    }

    #[test]
    fn fire_spec_with_salvo_size_lowers_to_fire_salvo() {
        let spec = PlatformCommandSpec::Fire {
            weapon_id: "w1".into(),
            track_id: "trk-1".into(),
            salvo_size: Some(2),
        };
        assert!(matches!(
            spec.to_platform_command("self"),
            PlatformCommand::FireSalvo {
                ref platform_id,
                ref weapon_id,
                ref track_id,
                salvo_size: 2,
            } if platform_id == "self" && weapon_id == "w1" && track_id == "trk-1"
        ));
    }

    #[test]
    fn coordinated_strike_binds_coordinator_to_platform_id() {
        let spec = PlatformCommandSpec::CoordinatedStrike {
            strike_platform_ids: vec!["uav-2".into()],
            target_id: "trk-9".into(),
            time_on_target_us: 1000,
        };
        match spec.to_platform_command("self") {
            PlatformCommand::CoordinatedStrike {
                coordinator_platform_id,
                ..
            } => assert_eq!(coordinator_platform_id, "self"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn valid_engage_mission_passes_validator() {
        let mission = engage_mission();
        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    }

    #[test]
    fn lethal_class_without_play_or_function_is_invalid() {
        let mut mission = engage_mission();
        mission.plays.clear();
        mission.functions.clear();
        let issues = mission.validate();
        assert!(issues.iter().any(|i| {
            i.rule == ValidationRule::R1 && i.message.contains("selected no tactical plays")
        }));
        assert!(issues.iter().any(|i| {
            i.rule == ValidationRule::R1 && i.message.contains("selected no executable functions")
        }));
    }

    #[test]
    fn r6_flags_lethal_without_approval_point() {
        let mut mission = engage_mission();
        mission.intervention_points.clear();
        let issues = mission.validate();
        assert!(issues.iter().any(|i| i.rule == ValidationRule::R6));
    }

    #[test]
    fn r4_flags_function_without_parent_play() {
        let mut mission = engage_mission();
        mission.functions[0].parent_play = "Nonexistent".into();
        let issues = mission.validate();
        assert!(issues.iter().any(|i| i.rule == ValidationRule::R4));
    }

    #[test]
    fn r3_flags_objective_without_feedback_var() {
        let mut mission = engage_mission();
        mission.objectives[0].feedback_var = None;
        let issues = mission.validate();
        assert!(issues.iter().any(|i| i.rule == ValidationRule::R3));
    }

    #[test]
    fn r7_flags_conflicting_fire_targets_on_one_platform() {
        let mut mission = engage_mission();
        mission
            .functions
            .push(lethal_function("fn-fire-2", "Engage", "self", "trk-2"));
        let issues = mission.validate();
        assert!(issues.iter().any(|i| i.rule == ValidationRule::R7));
    }

    #[test]
    fn display_renders_mission_block() {
        let rendered = engage_mission().to_string();
        assert!(rendered.starts_with("MISSION {"));
        assert!(rendered.contains("type: Engage"));
        assert!(rendered.contains("(lethal)"));
        assert!(rendered.contains("intervention_points"));
    }

    #[test]
    fn mission_roundtrips_as_json() {
        let mission = engage_mission();
        let json = serde_json::to_string(&mission).unwrap();
        let back: MissionDsl = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, MissionKind::Engage);
        assert_eq!(back.functions.len(), 1);
    }
}
