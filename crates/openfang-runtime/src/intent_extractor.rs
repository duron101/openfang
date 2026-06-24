//! Natural-language → [`StructuredIntent`] extraction (slow-loop entry).
//!
//! Two layers, LLM-first with deterministic fallback:
//!
//! - **Layer A — LLM strict schema** (optional): a semantic parser classifies the
//!   whole operator utterance before any keyword shortcut is trusted.
//! - **Layer B — keyword/regex fallback**: a tactical classification dictionary
//!   plus slot regexes (standoff distance, flank side, explicit platform/track
//!   ids, ROE). Fully deterministic and reproducible; no network.
//!
//! Entity grounding reuses [`DeterministicLabelResolver`]: semantic target
//! labels resolve to real snapshot track ids, and platform ids are validated
//! against the controlled set. Entities are never invented.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use openfang_types::config::PlatformControlPolicy;
use openfang_types::message::Message;
use openfang_types::mission_dsl::MissionKind;
use openfang_types::platform::{CcaRole, WorldSnapshot};
use openfang_types::semantic_frame::{
    ApproachSide, CommanderFrame, Effect, Environment, FrameConstraints, ObjectKind, ObjectRef,
    SubjectHint,
};
use openfang_types::umaa::WeaponReleaseLevel;
use serde::{Deserialize, Serialize};

use crate::llm_driver::{CompletionRequest, LlmDriver};
use crate::planning::{DeterministicLabelResolver, LabelResolveContext};

/// Which side to approach a target from when flanking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlankSide {
    Left,
    Right,
}

/// Parsed motion-control slots from natural language (heading, speed, flank).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ManeuverIntent {
    /// Absolute true heading in degrees `[0, 360)`.
    #[serde(default)]
    pub heading_deg: Option<f64>,
    /// Relative turn angle in degrees (positive = clockwise).
    #[serde(default)]
    pub heading_delta_deg: Option<f64>,
    /// Turn hint when no explicit delta/absolute heading is given.
    #[serde(default)]
    pub turn: Option<FlankSide>,
    /// Target speed in m/s.
    #[serde(default)]
    pub speed_ms: Option<f64>,
    /// Generate a flank-approach route when geometry allows.
    #[serde(default)]
    pub flank_approach: bool,
}

impl ManeuverIntent {
    pub fn is_active(&self) -> bool {
        self.heading_deg.is_some()
            || self.heading_delta_deg.is_some()
            || self.turn.is_some()
            || self.speed_ms.is_some()
            || self.flank_approach
    }

    /// Fill missing slots from a deterministic parse (LLM may omit motion fields).
    pub fn merge_deterministic(&mut self, other: &Self) {
        if self.heading_deg.is_none() {
            self.heading_deg = other.heading_deg;
        }
        if self.heading_delta_deg.is_none() {
            self.heading_delta_deg = other.heading_delta_deg;
        }
        if self.turn.is_none() {
            self.turn = other.turn;
        }
        if self.speed_ms.is_none() {
            self.speed_ms = other.speed_ms;
        }
        if !self.flank_approach {
            self.flank_approach = other.flank_approach;
        }
    }
}

/// Symbolic task emitted by the LLM planning layer. These are not executable
/// commands; the compiler must bind each task to trusted templates.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SymbolicTask {
    pub task_id: String,
    #[serde(default)]
    pub platform: Option<String>,
    pub action: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub criteria: Option<String>,
    #[serde(default)]
    pub preconditions: Vec<String>,
    #[serde(default)]
    pub parameters: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub phase: u32,
    #[serde(default)]
    pub ordering: u32,
}

/// Which parser produced the accepted structured intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntentSemanticSource {
    /// LLM strict-schema parse accepted after deterministic validation.
    Llm,
    /// Deterministic keyword/regex fallback accepted.
    #[default]
    Deterministic,
}

/// Structured, grounded representation of a commander's natural-language intent.
/// This is the intermediate the [`crate::mission_compiler`] compiles into a
/// `MissionDsl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredIntent {
    /// Original free-form text.
    pub raw_text: String,
    /// Classified mission class.
    pub kind: MissionKind,
    /// Flank approach side, if the intent implies one.
    #[serde(default)]
    pub flank_side: Option<FlankSide>,
    /// Requested standoff distance in meters, if specified.
    #[serde(default)]
    pub standoff_m: Option<f64>,
    /// Requested patrol area radius in meters, if specified.
    #[serde(default)]
    pub patrol_radius_m: Option<f64>,
    /// ROE preference, if specified.
    #[serde(default)]
    pub roe: Option<WeaponReleaseLevel>,
    /// Raw semantic target labels for grounding (e.g. "蓝方指挥所").
    #[serde(default)]
    pub target_labels: Vec<String>,
    /// Grounded / explicit target track ids.
    #[serde(default)]
    pub target_track_ids: Vec<String>,
    /// Explicit controlled platform ids referenced by the operator.
    #[serde(default)]
    pub platform_ids: Vec<String>,
    /// Whether the operator addressed "all (uav)s".
    #[serde(default)]
    pub all_platforms: bool,
    /// Role hints parsed from the text (recon, strike, …).
    #[serde(default)]
    pub role_hints: Vec<CcaRole>,
    /// Motion-control slots (heading, speed, flank approach).
    #[serde(default)]
    pub maneuver: ManeuverIntent,
    /// Explicit sensor id or semantic sensor hint (e.g. "radar", "eoir").
    #[serde(default)]
    pub sensor_id: Option<String>,
    /// Explicit desired sensor mode/action ("on", "off", "search", "track").
    #[serde(default)]
    pub sensor_mode: Option<String>,
    /// LLM-generated symbolic task graph with preconditions.
    #[serde(default)]
    pub task_plan: Vec<SymbolicTask>,
    /// Extraction confidence in `[0, 1]`.
    pub confidence: f64,
    /// Human-readable rationale for the classification.
    pub rationale: String,
    /// Parser source accepted by the extractor.
    #[serde(default)]
    pub semantic_source: IntentSemanticSource,
    /// Why the LLM semantic layer fell back, if it did.
    #[serde(default)]
    pub fallback_reason: Option<String>,
}

impl StructuredIntent {
    /// A safe, low-confidence `Unknown` intent that carries no actuation.
    pub fn unknown(raw_text: impl Into<String>) -> Self {
        Self {
            raw_text: raw_text.into(),
            kind: MissionKind::Unknown,
            flank_side: None,
            standoff_m: None,
            patrol_radius_m: None,
            roe: None,
            target_labels: Vec::new(),
            target_track_ids: Vec::new(),
            platform_ids: Vec::new(),
            all_platforms: false,
            role_hints: Vec::new(),
            maneuver: ManeuverIntent::default(),
            sensor_id: None,
            sensor_mode: None,
            task_plan: Vec::new(),
            confidence: 0.0,
            rationale: "unclassified intent".into(),
            semantic_source: IntentSemanticSource::Deterministic,
            fallback_reason: None,
        }
    }

    /// Whether the intent has at least one grounded target track.
    pub fn has_target(&self) -> bool {
        !self.target_track_ids.is_empty()
    }
}

impl From<&CommanderFrame> for StructuredIntent {
    fn from(frame: &CommanderFrame) -> Self {
        let mut intent = StructuredIntent::unknown(&frame.raw_text);
        intent.kind = frame.effect.mission_kind();
        intent.flank_side = frame.environment.approach.map(|side| match side {
            ApproachSide::Left => FlankSide::Left,
            ApproachSide::Right => FlankSide::Right,
        });
        intent.standoff_m = frame.environment.standoff_m;
        intent.roe = frame.constraints.roe;
        intent.target_labels = frame
            .objects
            .iter()
            .filter_map(|object| object.label.clone())
            .collect();
        intent.target_track_ids = frame
            .objects
            .iter()
            .filter_map(|object| object.track_id.clone())
            .collect();
        intent.platform_ids = frame
            .subject_hints
            .iter()
            .filter_map(|hint| hint.platform_id.clone())
            .collect();
        intent.all_platforms = frame.subject_hints.iter().any(|hint| hint.all_platforms);
        intent.role_hints = frame
            .subject_hints
            .iter()
            .filter_map(|hint| hint.role)
            .collect();
        intent.confidence = frame.confidence;
        intent.rationale = frame.rationale.clone();
        intent.semantic_source = match frame.semantic_source.as_str() {
            "llm" => IntentSemanticSource::Llm,
            _ => IntentSemanticSource::Deterministic,
        };
        intent
    }
}

pub fn to_commander_frame(intent: &StructuredIntent) -> CommanderFrame {
    let effect = effect_for_mission_kind(intent.kind, &intent.raw_text);
    let lower = intent.raw_text.to_lowercase();
    let mut objects: Vec<ObjectRef> = intent
        .target_labels
        .iter()
        .map(|label| ObjectRef {
            kind: ObjectKind::Label,
            label: Some(label.clone()),
            track_id: None,
            area: None,
        })
        .collect();
    for track_id in &intent.target_track_ids {
        if !objects
            .iter()
            .any(|object| object.track_id.as_deref() == Some(track_id.as_str()))
        {
            objects.push(ObjectRef {
                kind: ObjectKind::Track,
                label: None,
                track_id: Some(track_id.clone()),
                area: None,
            });
        }
    }
    if contains_any(
        &lower,
        &[
            "侦察无人机",
            "侦查无人机",
            "scout uav",
            "recon uav",
            "reconnaissance uav",
        ],
    ) && !objects.iter().any(|object| {
        object.kind == ObjectKind::Asset && object.label.as_deref() == Some("scout_uav_slot")
    }) {
        objects.push(ObjectRef {
            kind: ObjectKind::Asset,
            label: Some("scout_uav_slot".into()),
            track_id: None,
            area: None,
        });
    }

    let mut subject_hints: Vec<SubjectHint> = intent
        .platform_ids
        .iter()
        .map(|platform_id| SubjectHint {
            platform_id: Some(platform_id.clone()),
            role: None,
            all_platforms: false,
        })
        .collect();
    for role in &intent.role_hints {
        subject_hints.push(SubjectHint {
            platform_id: None,
            role: Some(*role),
            all_platforms: false,
        });
    }
    if intent.all_platforms {
        subject_hints.push(SubjectHint {
            platform_id: None,
            role: None,
            all_platforms: true,
        });
    }

    CommanderFrame {
        raw_text: intent.raw_text.clone(),
        effect,
        objects,
        environment: Environment {
            area: None,
            approach: intent.flank_side.map(|side| match side {
                FlankSide::Left => ApproachSide::Left,
                FlankSide::Right => ApproachSide::Right,
            }),
            standoff_m: intent.standoff_m,
        },
        constraints: FrameConstraints {
            roe: intent.roe,
            time_window: None,
            allow_degrade: false,
            pid_required: false,
        },
        subject_hints,
        confidence: intent.confidence,
        rationale: intent.rationale.clone(),
        semantic_source: match intent.semantic_source {
            IntentSemanticSource::Llm => "llm",
            IntentSemanticSource::Deterministic => "deterministic",
        }
        .into(),
    }
}

fn effect_for_mission_kind(kind: MissionKind, raw_text: &str) -> Effect {
    let lower = raw_text.to_lowercase();
    match kind {
        MissionKind::Engage | MissionKind::CoordinatedStrike | MissionKind::ReconFlankStrike => {
            if contains_any(&lower, &["压制", "suppress"]) {
                Effect::Suppress
            } else {
                Effect::Destroy
            }
        }
        MissionKind::Recon => {
            if contains_any(&lower, &["监视", "监控", "surveil", "monitor"]) {
                Effect::Surveil
            } else {
                Effect::Reconnoiter
            }
        }
        MissionKind::Track | MissionKind::TargetingHandoff => Effect::Track,
        MissionKind::Patrol | MissionKind::Picket => Effect::Screen,
        MissionKind::Rtb => Effect::ReturnToBase,
        MissionKind::PointDefense => Effect::Defend,
        MissionKind::Escort => Effect::Escort,
        MissionKind::MaritimeInterdiction => Effect::Interdict,
        MissionKind::Deception => Effect::Deceive,
        MissionKind::SensorControl => Effect::Unknown,
        MissionKind::ReactiveDefense => Effect::Evade,
        MissionKind::Unknown => Effect::Unknown,
    }
}

/// Owned context for the LLM extractor layer (crosses `.await`).
#[derive(Debug, Clone)]
pub struct ExtractContext {
    pub raw_text: String,
    /// Deterministic fallback result, offered to the model as an optional hint.
    pub keyword_result: StructuredIntent,
    /// Candidate target track ids the model may reference (never invent).
    pub candidate_track_ids: Vec<String>,
    /// Controlled platform ids the model may reference.
    pub controlled_platform_ids: Vec<String>,
    /// Rich target metadata for LLM grounding.
    pub candidate_targets: Vec<serde_json::Value>,
    /// Rich platform metadata for LLM grounding.
    pub controlled_platforms: Vec<serde_json::Value>,
}

/// Pluggable LLM extractor. Implemented by the kernel with an LLM backend;
/// tests use deterministic fakes. Returning `None` degrades to Layer A.
#[async_trait]
pub trait IntentExtractDriver: Send + Sync {
    async fn extract(&self, ctx: ExtractContext) -> Option<StructuredIntent>;
}

/// `mc` agent planning doctrine, bundled at compile time from
/// `tactical-assets/agents/mc/promt.md`. Its §A "ArkSIM 可执行指令规范" mirrors
/// [`INTENT_EXTRACT_SYSTEM_PROMPT`]; the rest provides strategic framing. Used
/// as an optional system-prompt prefix (see [`LlmIntentExtractDriver::with_doctrine`]).
pub const MC_PLANNING_DOCTRINE: &str =
    include_str!("../../../tactical-assets/agents/mc/promt.md");

/// The bundled `mc/promt.md` planning doctrine (see [`MC_PLANNING_DOCTRINE`]).
pub fn mc_planning_doctrine() -> &'static str {
    MC_PLANNING_DOCTRINE
}

/// LLM-backed [`IntentExtractDriver`] that requests a strict symbolic mission
/// plan and leaves all executable binding to the deterministic compiler.
pub struct LlmIntentExtractDriver {
    driver: Arc<dyn LlmDriver>,
    model: String,
    timeout: Duration,
    /// Optional doctrine prefix (e.g. `mc/promt.md`) prepended to the strict
    /// system contract. The contract is always appended last so it wins on any
    /// conflict about the output format.
    doctrine: Option<String>,
}

impl LlmIntentExtractDriver {
    pub fn new(driver: Arc<dyn LlmDriver>, model: impl Into<String>, timeout: Duration) -> Self {
        Self {
            driver,
            model: model.into(),
            timeout,
            doctrine: None,
        }
    }

    /// Attach an optional doctrine prefix (e.g. `mc/promt.md`) to the system
    /// prompt. `None` keeps the strict contract-only behavior.
    pub fn with_doctrine(mut self, doctrine: Option<String>) -> Self {
        self.doctrine = doctrine.filter(|d| !d.trim().is_empty());
        self
    }

    /// Compose the system prompt: optional doctrine prefix followed by the
    /// strict, ArkSIM-aligned JSON contract (which always has final say).
    fn system_prompt(&self) -> String {
        match &self.doctrine {
            Some(doctrine) => format!(
                "{doctrine}\n\n---\n\nThe following machine contract OVERRIDES any conflicting guidance above and defines the ONLY valid output. {INTENT_EXTRACT_SYSTEM_PROMPT}"
            ),
            None => INTENT_EXTRACT_SYSTEM_PROMPT.to_string(),
        }
    }
}

#[async_trait]
impl IntentExtractDriver for LlmIntentExtractDriver {
    async fn extract(&self, ctx: ExtractContext) -> Option<StructuredIntent> {
        let request = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::user(intent_extract_user_prompt(&ctx))],
            tools: Vec::new(),
            max_tokens: 1400,
            temperature: 0.0,
            system: Some(self.system_prompt()),
            thinking: None,
        };
        let text = tokio::time::timeout(self.timeout, self.driver.complete(request))
            .await
            .ok()?
            .ok()?
            .text();
        parse_llm_structured_intent(&text, &ctx.raw_text)
    }
}

const INTENT_EXTRACT_SYSTEM_PROMPT: &str = r#"You are OpenFang's mission DSL planning compiler.
Return exactly one JSON object. Do not return markdown.
Your primary output is mission_plan.tasks: a trusted symbolic task DAG.
You must not invent executable commands or arbitrary actions.
You may include target_track_ids or platform_ids only if they appear in the provided candidate lists.
Use "candidate_targets" and "controlled_platforms" in the user payload to understand the names, types, and weapons of the targets and platforms in the scenario so you can ground the user's natural language objective accurately.
If unsure, return kind="unknown", mission_plan.tasks=[] with low confidence.

Allowed symbolic actions only, aligned with ArkSIM ActionsFromOutside proto mappings:
- FollowRoute -> PlatformCommand::FollowRoute -> proto field a_followroute: parameters { "route_shape": "circle|polyline", "center": "current_position|target|latlon", "radius_m": number|null, "waypoint_count": number|null, "waypoints": [{"lat": number, "lon": number, "alt": number|null}]|null, "speed": "max|cruise"|number|null }
- Goto -> PlatformCommand::GotoLocation -> proto field a_gotolocation: parameters { "target": "track_id|area|latlon|null", "lat": number|null, "lon": number|null, "alt": number|null, "speed": "max|cruise"|number|null }
- SetHeading -> PlatformCommand::SetHeading -> proto field a_desiredheading: parameters { "heading_deg": number, "speed": "max|cruise"|number|null, "turn": "left|right|null" }
- SetSpeed -> PlatformCommand::SetSpeed -> proto field a_desiredvelocity: parameters { "speed": "max|cruise"|number }
- SensorOn -> PlatformCommand::SensorOn -> proto field a_sensoraction/E_TurnOnSensor: parameters { "sensor": "radar|eoir|esm|default" }
- SensorOff -> PlatformCommand::SensorOff -> proto field a_sensoraction/E_TurnOffSensor: parameters { "sensor": "radar|eoir|esm|default" }
- SensorSetMode -> PlatformCommand::SensorSetMode -> proto field a_changesensormode: parameters { "sensor": "radar|eoir|esm|default", "mode": "search|track|passive|active" }
- Designate -> PlatformCommand::UpdateTarget -> proto field a_sensoraction/E_UpdateTarget: parameters { "target_track_id": "id from candidate_track_ids only" }
- Fire -> PlatformCommand::FireAtTarget or FireSalvo -> proto field a_fireattarget/a_firesalvo: parameters { "target_track_id": "id from candidate_track_ids only", "salvo_size": number|null }
- Jam -> PlatformCommand::JamStart -> proto field a_changejammingmode: parameters { "frequency_hz": number|null, "bandwidth_hz": number|null, "target_track_id": "id from candidate_track_ids or null" }
- JamStop -> PlatformCommand::JamStop -> proto field a_sensoraction/E_StopJamming: parameters {}
- SendMessage -> PlatformCommand::SendMessage -> proto field a_sendmsgtoplatform: parameters { "to_platform_id": "id from controlled_platform_ids or candidate target platform id", "message": "short operator/audit message" }
USV-specific boundary: do not output SetAltitude for unmanned surface vessels. SetOutsideControl/ReleaseOutsideControl and ChangeCommander are system-control actions injected by OpenFang, not LLM task actions.
Do not output unsupported or unsafe task actions such as LaunchUav, RecoverUav, CoordinatedStrike, ReleaseDecoy/FireChaff, WeaponSafeAll, CommOn/CommOff, AuxCommand, ChangePlatformNumber, SendMsgToCommandChain, formation, deck, or relay commands unless ArkSIM command_mapper::is_supported() gains a safe wire mapping and the target platform type supports it.

Task graph rules:
- Every task requires task_id, platform, action, parameters, preconditions, criteria, phase, ordering.
- Serial dependency: use preconditions like ["T1_complete"].
- Parallel tasks: share the same preconditions and do not depend on each other.
- Event triggers: use preconditions like ["event:missile_inbound"].
- Closed-loop criteria must be machine-checkable: route_started, route_completed, position_reached, speed_set, heading_set, sensor_active, sensor_mode_set, jammer_active, jammer_stopped, target_updated, target_destroyed, message_sent.
- phase and ordering are for audit/readable order only. Execution order is derived from preconditions.
- Do not output weapon_id, jammer_id, or hidden sensor ids. The compiler binds real components.

JSON schema:
{
  "effect": "reconnoiter|surveil|track|suppress|destroy|escort|screen|deceive|defend|evade|interdict|return_to_base|unknown",
  "objects": [
    {
      "kind": "track|label|area|asset|unknown",
      "label": "semantic target or deployable asset label|null",
      "track_id": "id from candidate_track_ids only|null",
      "area": null
    }
  ],
  "environment": {
    "area": null,
    "approach": "left|right|null",
    "standoff_m": 3000.0|null
  },
  "constraints": {
    "roe": "weapons_free|weapons_tight|weapons_hold|null",
    "time_window": null,
    "allow_degrade": false,
    "pid_required": false
  },
  "subject_hints": [
    {
      "platform_id": "id from controlled_platform_ids only|null",
      "role": "recon|striker|designator|relay|decoy|intercept|patrol|escort|surveil|leader|adaptive|ew_protection|ew_jamming|null",
      "all_platforms": false
    }
  ],
  "kind": "engage|recon_flank_strike|coordinated_strike|recon|patrol|rtb|track|point_defense|targeting_handoff|picket|escort|maritime_interdiction|deception|sensor_control|reactive_defense|unknown",
  "flank_side": "left|right|null",
  "standoff_m": 3000.0|null,
  "patrol_radius_m": 100000.0|null,
  "roe": "weapons_free|weapons_tight|weapons_hold|null",
  "target_labels": ["semantic target labels in the user's language"],
  "target_track_ids": ["ids from candidate_track_ids only"],
  "platform_ids": ["ids from controlled_platform_ids only"],
  "all_platforms": false,
  "role_hints": ["recon|striker|designator|jammer|escort|decoy|relay"],
  "maneuver": {
    "heading_deg": 270.0|null,
    "heading_delta_deg": 45.0|null,
    "turn": "left|right|null",
    "speed_ms": 8.0|null,
    "flank_approach": false
  },
  "mission_plan": {
    "platform": "single|heterogeneous",
    "description": "short mission description",
    "tasks": [
      {
        "task_id": "T1",
        "platform": "id from controlled_platform_ids or role label like UAV/USV|null",
        "action": "FollowRoute|Goto|SetHeading|SetSpeed|SensorOn|SensorOff|SensorSetMode|Designate|Fire|Jam|JamStop|SendMessage",
        "target": "id from candidate_track_ids, semantic label, or area label|null",
        "criteria": "machine-checkable completion criterion|null",
        "preconditions": ["T0_complete or event:missile_inbound"],
        "parameters": {"route_shape": "circle", "center": "current_position", "radius_m": 100000, "speed": "cruise"},
        "phase": 0,
        "ordering": 0
      }
    ]
  },
  "confidence": 0.0,
  "rationale": "short reason"
}
"#;

fn intent_extract_user_prompt(ctx: &ExtractContext) -> String {
    let payload = serde_json::json!({
        "objective": ctx.raw_text,
        "candidate_track_ids": ctx.candidate_track_ids,
        "controlled_platform_ids": ctx.controlled_platform_ids,
        "candidate_targets": ctx.candidate_targets,
        "controlled_platforms": ctx.controlled_platforms,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
}

#[derive(Debug, Deserialize)]
struct LlmStructuredIntent {
    #[serde(default)]
    effect: Option<Effect>,
    #[serde(default)]
    objects: Vec<ObjectRef>,
    #[serde(default)]
    environment: Option<Environment>,
    #[serde(default)]
    constraints: Option<FrameConstraints>,
    #[serde(default)]
    subject_hints: Vec<SubjectHint>,
    kind: Option<MissionKind>,
    #[serde(default)]
    flank_side: Option<FlankSide>,
    #[serde(default)]
    standoff_m: Option<f64>,
    #[serde(default)]
    patrol_radius_m: Option<f64>,
    #[serde(default)]
    roe: Option<WeaponReleaseLevel>,
    #[serde(default)]
    target_labels: Vec<String>,
    #[serde(default)]
    target_track_ids: Vec<String>,
    #[serde(default)]
    platform_ids: Vec<String>,
    #[serde(default)]
    all_platforms: bool,
    #[serde(default)]
    role_hints: Vec<CcaRole>,
    #[serde(default)]
    maneuver: ManeuverIntent,
    #[serde(default)]
    mission_plan: Option<LlmMissionPlan>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    rationale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LlmMissionPlan {
    #[serde(default)]
    tasks: Vec<SymbolicTask>,
}

fn parse_llm_structured_intent(text: &str, raw_text: &str) -> Option<StructuredIntent> {
    let json = extract_json_object(text)?;
    let parsed: LlmStructuredIntent = serde_json::from_str(&json).ok()?;
    let confidence = parsed.confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let task_plan = sanitize_symbolic_tasks(
        parsed
            .mission_plan
            .map(|plan| plan.tasks)
            .unwrap_or_default(),
    );
    if parsed.effect.is_some()
        || !parsed.objects.is_empty()
        || parsed.environment.is_some()
        || parsed.constraints.is_some()
        || !parsed.subject_hints.is_empty()
    {
        let frame = CommanderFrame {
            raw_text: raw_text.to_string(),
            effect: parsed.effect.unwrap_or_else(|| {
                parsed
                    .kind
                    .map(|kind| effect_for_mission_kind(kind, raw_text))
                    .unwrap_or_default()
            }),
            objects: parsed
                .objects
                .into_iter()
                .map(sanitize_object_ref)
                .filter(|object| {
                    object.label.is_some() || object.track_id.is_some() || object.area.is_some()
                })
                .collect(),
            environment: parsed.environment.unwrap_or_default(),
            constraints: parsed.constraints.unwrap_or_default(),
            subject_hints: parsed
                .subject_hints
                .into_iter()
                .map(sanitize_subject_hint)
                .collect(),
            confidence,
            rationale: parsed
                .rationale
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| "llm semantic frame parse".into()),
            semantic_source: "llm".into(),
        };
        let mut intent: StructuredIntent = (&frame).into();
        intent.patrol_radius_m = parsed.patrol_radius_m.filter(|v| v.is_finite() && *v > 0.0);
        intent.task_plan = task_plan;
        return Some(intent);
    }

    Some(StructuredIntent {
        raw_text: raw_text.to_string(),
        kind: parsed.kind.unwrap_or(MissionKind::Unknown),
        flank_side: parsed.flank_side,
        standoff_m: parsed.standoff_m.filter(|v| v.is_finite() && *v >= 0.0),
        patrol_radius_m: parsed.patrol_radius_m.filter(|v| v.is_finite() && *v > 0.0),
        roe: parsed.roe,
        target_labels: parsed
            .target_labels
            .into_iter()
            .map(|label| label.trim().to_string())
            .filter(|label| !label.is_empty())
            .collect(),
        target_track_ids: parsed
            .target_track_ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect(),
        platform_ids: parsed
            .platform_ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect(),
        all_platforms: parsed.all_platforms,
        role_hints: parsed.role_hints,
        maneuver: parsed.maneuver,
        sensor_id: None,
        sensor_mode: None,
        task_plan,
        confidence,
        rationale: parsed
            .rationale
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "llm semantic parse".into()),
        semantic_source: IntentSemanticSource::Llm,
        fallback_reason: None,
    })
}

fn sanitize_symbolic_tasks(tasks: Vec<SymbolicTask>) -> Vec<SymbolicTask> {
    tasks
        .into_iter()
        .filter_map(|mut task| {
            task.task_id = task.task_id.trim().to_string();
            task.action = task.action.trim().to_string();
            task.platform = task
                .platform
                .map(|platform| platform.trim().to_string())
                .filter(|platform| !platform.is_empty());
            task.target = task
                .target
                .map(|target| target.trim().to_string())
                .filter(|target| !target.is_empty());
            task.criteria = task
                .criteria
                .map(|criteria| criteria.trim().to_string())
                .filter(|criteria| !criteria.is_empty());
            task.preconditions = task
                .preconditions
                .into_iter()
                .map(|precondition| precondition.trim().to_string())
                .filter(|precondition| !precondition.is_empty())
                .collect();
            task.phase = task.phase.min(99);
            task.ordering = task.ordering.min(999);
            if task.task_id.is_empty() || task.action.is_empty() {
                None
            } else {
                Some(task)
            }
        })
        .collect()
}

fn sanitize_object_ref(mut object: ObjectRef) -> ObjectRef {
    object.label = object
        .label
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty());
    object.track_id = object
        .track_id
        .map(|track_id| track_id.trim().to_string())
        .filter(|track_id| !track_id.is_empty());
    object
}

fn sanitize_subject_hint(mut hint: SubjectHint) -> SubjectHint {
    hint.platform_id = hint
        .platform_id
        .map(|platform_id| platform_id.trim().to_string())
        .filter(|platform_id| !platform_id.is_empty());
    hint
}

fn extract_json_object(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed.to_string());
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (start < end).then(|| trimmed[start..=end].to_string())
}

/// The two-layer intent extractor.
#[derive(Debug, Default, Clone)]
pub struct IntentExtractor {
    resolver: DeterministicLabelResolver,
}

impl IntentExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Deterministic Layer-A extraction + entity grounding. Always succeeds with
    /// a (possibly `Unknown`) result; never invents entities.
    pub fn extract(
        &self,
        text: &str,
        snapshot: &WorldSnapshot,
        policy: &PlatformControlPolicy,
    ) -> StructuredIntent {
        let mut intent = classify_keyword(text);
        self.ground(&mut intent, snapshot, policy);
        intent
    }

    /// Deterministic extraction as a Tier-1 commander frame.
    pub fn extract_frame(
        &self,
        text: &str,
        snapshot: &WorldSnapshot,
        policy: &PlatformControlPolicy,
    ) -> CommanderFrame {
        let intent = self.extract(text, snapshot, policy);
        to_commander_frame(&intent)
    }

    /// LLM-required extraction for DSL compilation. Deterministic extraction is
    /// kept only as a diagnostic context; it must never become an executable DSL
    /// fallback when the LLM fails to produce a valid task graph.
    pub async fn extract_with_llm(
        &self,
        text: &str,
        snapshot: &WorldSnapshot,
        policy: &PlatformControlPolicy,
        driver: Option<&dyn IntentExtractDriver>,
        min_confidence: f64,
    ) -> StructuredIntent {
        let mut keyword_result = self.extract(text, snapshot, policy);
        keyword_result.semantic_source = IntentSemanticSource::Deterministic;
        let Some(driver) = driver else {
            return llm_failed_intent(text, "llm driver unavailable");
        };

        let candidate_track_ids = candidate_track_ids(snapshot, policy);
        let controlled_platform_ids = controlled_platform_ids(snapshot, policy);
        let candidate_targets = candidate_targets_json(snapshot, policy);
        let controlled_platforms = controlled_platforms_json(snapshot, policy);
        let ctx = ExtractContext {
            raw_text: text.to_string(),
            keyword_result: keyword_result.clone(),
            candidate_track_ids: candidate_track_ids.clone(),
            controlled_platform_ids: controlled_platform_ids.clone(),
            candidate_targets,
            controlled_platforms,
        };

        match driver.extract(ctx).await {
            Some(mut llm) => {
                // Never trust the model's entities: re-validate against reality.
                llm.raw_text = text.to_string();
                let original_track_count = llm.target_track_ids.len();
                let original_platform_count = llm.platform_ids.len();
                llm.target_track_ids
                    .retain(|id| candidate_track_ids.iter().any(|c| c == id));
                llm.platform_ids
                    .retain(|id| controlled_platform_ids.iter().any(|c| c == id));
                self.ground(&mut llm, snapshot, policy);
                llm.semantic_source = IntentSemanticSource::Llm;
                if llm.kind == MissionKind::Unknown {
                    llm_failed_intent(text, "llm returned unknown mission kind")
                } else if llm.task_plan.is_empty() {
                    llm_failed_intent(text, "llm returned no executable task graph")
                } else if llm.confidence < min_confidence {
                    llm_failed_intent(
                        text,
                        format!("llm confidence {:.2} below threshold", llm.confidence),
                    )
                } else if original_track_count > 0 && llm.target_track_ids.is_empty() {
                    llm_failed_intent(text, "llm referenced no valid target tracks")
                } else if original_platform_count > 0 && llm.platform_ids.is_empty() {
                    llm_failed_intent(text, "llm referenced no valid controlled platforms")
                } else {
                    llm
                }
            }
            None => llm_failed_intent(text, "llm extraction failed"),
        }
    }

    /// LLM-first extraction as a Tier-1 commander frame. This preserves the
    /// current LLM driver contract while giving callers a normalized frame.
    pub async fn extract_frame_with_llm(
        &self,
        text: &str,
        snapshot: &WorldSnapshot,
        policy: &PlatformControlPolicy,
        driver: Option<&dyn IntentExtractDriver>,
        min_confidence: f64,
    ) -> CommanderFrame {
        let intent = self
            .extract_with_llm(text, snapshot, policy, driver, min_confidence)
            .await;
        to_commander_frame(&intent)
    }

    /// Ground semantic labels → real track ids and validate platform ids.
    /// Expands `all_platforms` into the controlled set.
    pub fn ground(
        &self,
        intent: &mut StructuredIntent,
        snapshot: &WorldSnapshot,
        policy: &PlatformControlPolicy,
    ) {
        if !intent.target_labels.is_empty() {
            let resolutions = self.resolver.resolve(LabelResolveContext {
                snapshot,
                labels: &intent.target_labels,
                control_policy: policy,
            });
            for resolution in resolutions {
                if let Some(track_id) = resolution.selected_track_id {
                    if !intent.target_track_ids.contains(&track_id) {
                        intent.target_track_ids.push(track_id);
                    }
                }
            }
        }

        let controlled = controlled_platform_ids(snapshot, policy);
        if intent.all_platforms {
            for id in &controlled {
                if !intent.platform_ids.contains(id) {
                    intent.platform_ids.push(id.clone());
                }
            }
        }
        // Drop platform ids that are not in the controlled set (when known).
        if !controlled.is_empty() {
            intent
                .platform_ids
                .retain(|id| controlled.iter().any(|c| c == id));
        }
    }
}

fn llm_failed_intent(text: &str, reason: impl Into<String>) -> StructuredIntent {
    let mut intent = StructuredIntent::unknown(text);
    intent.semantic_source = IntentSemanticSource::Llm;
    intent.fallback_reason = Some(reason.into());
    intent.rationale = "llm task graph unavailable".into();
    intent
}

/// Track ids visible anywhere in the snapshot — the only valid grounding targets.
/// Non-controlled (hostile/neutral) platform ids are also addressable as targets
/// (mirroring [`DeterministicLabelResolver`]); controlled own-side platforms are
/// never offered as targets.
fn candidate_track_ids(snapshot: &WorldSnapshot, policy: &PlatformControlPolicy) -> Vec<String> {
    let mut ids = Vec::new();
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if !ids.contains(&track.track_id) {
                ids.push(track.track_id.clone());
            }
        }
    }
    for platform in &snapshot.platforms {
        if !policy.controlled_side.matches(platform.affiliation) && !ids.contains(&platform.id) {
            ids.push(platform.id.clone());
        }
    }
    ids
}

/// Controlled platform ids: the explicit allow-list, else every platform on the
/// controlled side, plus the configured own platform id.
fn controlled_platform_ids(
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if !policy.controlled_platforms.is_empty() {
        ids = policy.controlled_platforms.clone();
    } else {
        for platform in &snapshot.platforms {
            if policy.controlled_side.matches(platform.affiliation) && !ids.contains(&platform.id) {
                ids.push(platform.id.clone());
            }
        }
    }
    if !policy.own_platform_id.is_empty() && !ids.contains(&policy.own_platform_id) {
        ids.push(policy.own_platform_id.clone());
    }
    ids
}

fn candidate_targets_json(
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
) -> Vec<serde_json::Value> {
    let mut targets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if !seen.insert(track.track_id.clone()) {
                continue;
            }
            targets.push(serde_json::json!({
                "track_id": track.track_id,
                "target_name": track.target_name,
                "classification": track.classification,
                "affiliation": format!("{:?}", track.affiliation),
                "iff": track.iff,
                "range_m": track.range_m,
                "is_active": track.is_active
            }));
        }
    }
    for platform in &snapshot.platforms {
        if !policy.controlled_side.matches(platform.affiliation) && seen.insert(platform.id.clone())
        {
            targets.push(serde_json::json!({
                "track_id": platform.id,
                "target_name": platform.name,
                "classification": platform.platform_type,
                "affiliation": format!("{:?}", platform.affiliation),
                "iff": "foe",
                "range_m": serde_json::Value::Null,
                "is_active": true
            }));
        }
    }
    targets
}

fn controlled_platforms_json(
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
) -> Vec<serde_json::Value> {
    let mut platforms = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut ids: Vec<String> = Vec::new();
    if !policy.controlled_platforms.is_empty() {
        ids = policy.controlled_platforms.clone();
    } else {
        for platform in &snapshot.platforms {
            if policy.controlled_side.matches(platform.affiliation) && !ids.contains(&platform.id) {
                ids.push(platform.id.clone());
            }
        }
    }
    if !policy.own_platform_id.is_empty() && !ids.contains(&policy.own_platform_id) {
        ids.push(policy.own_platform_id.clone());
    }

    for id in ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(p) = snapshot.platforms.iter().find(|platform| platform.id == id) {
            platforms.push(serde_json::json!({
                "platform_id": p.id,
                "name": p.name,
                "type": p.platform_type,
                "affiliation": format!("{:?}", p.affiliation),
                "damage": p.damage,
                "weapons": p.onboard_weapons.iter().map(|w| {
                    serde_json::json!({
                        "weapon_id": w.weapon_id,
                        "quantity": w.quantity_remaining,
                        "is_ready": w.is_ready
                    })
                }).collect::<Vec<_>>()
            }));
        } else {
            platforms.push(serde_json::json!({
                "platform_id": id,
                "name": "unknown",
                "type": "unknown",
                "affiliation": "unknown",
                "damage": 0.0,
                "weapons": []
            }));
        }
    }
    platforms
}

// ─────────────────────────────────────────────
// Layer A — keyword/regex classification
// ─────────────────────────────────────────────

fn classify_keyword(text: &str) -> StructuredIntent {
    let lower = text.to_lowercase();
    let mut intent = StructuredIntent::unknown(text);

    // ── Slots (parsed regardless of class) ──
    intent.standoff_m = parse_standoff_m(&lower);
    intent.patrol_radius_m = parse_patrol_radius_m(&lower);
    intent.flank_side = parse_flank_side(&lower);
    intent.roe = parse_roe(&lower);
    intent.platform_ids = parse_platform_ids(&lower);
    intent.target_track_ids = parse_track_ids(&lower);
    intent.all_platforms = contains_any(
        &lower,
        &["所有无人机", "全部无人机", "all uav", "all drones"],
    );
    intent.target_labels = parse_target_labels(text);
    intent.role_hints = parse_role_hints(&lower);
    intent.maneuver = parse_maneuver_slots(&lower);

    if let Some((sensor_id, sensor_mode)) = parse_explicit_sensor_control(&lower) {
        intent.kind = MissionKind::SensorControl;
        intent.sensor_id = Some(sensor_id);
        intent.sensor_mode = Some(sensor_mode);
        intent.confidence = 0.95;
        intent.rationale = "explicit sensor control keyword".into();
        intent.target_labels.retain(|label| !is_sensor_label(label));
        return intent;
    }

    // ── Classification (ordered: most specific first) ──
    let has_flank = intent.flank_side.is_some()
        || intent.maneuver.flank_approach
        || contains_any(&lower, &["绕后", "侧翼", "迂回", "flank", "envelop"]);
    let has_recon = contains_any(
        &lower,
        &[
            "侦察", "侦查", "情报", "isr", "recon", "scout", "监视", "surveil",
        ],
    );
    let has_recon_uav_deploy = explicit_recon_uav_deploy(&lower);
    let has_strike = contains_any(
        &lower,
        &[
            "杀伤", "摧毁", "消灭", "歼灭", "打击", "攻击", "察打", "strike", "engage", "开火",
            "fire", "attack", "命中",
        ],
    );
    let has_coordinated =
        contains_any(&lower, &["协同", "一体", "coordinated", "时敏", "联合打击"]);
    let has_patrol = contains_any(&lower, &["巡逻", "巡航", "patrol", "cap"]);
    let has_rtb = contains_any(
        &lower,
        &["返航", "回收", "返回基地", "rtb", "return to base", "返场"],
    );
    let has_track_only = contains_any(
        &lower,
        &["只跟踪", "仅跟踪", "保持跟踪", "track only", "shadow"],
    );
    let has_evade = contains_any(
        &lower,
        &[
            "规避",
            "躲避",
            "机动规避",
            "闪避",
            "evade",
            "evasive",
            "defensive maneuver",
        ],
    );
    let has_soft_kill = contains_any(
        &lower,
        &[
            "干扰",
            "电子战",
            "箔条",
            "诱饵",
            "软杀伤",
            "jam",
            "jamming",
            "chaff",
            "decoy",
            "soft-kill",
        ],
    );
    let weapons_hold = matches!(intent.roe, Some(WeaponReleaseLevel::WeaponsHold))
        || contains_any(
            &lower,
            &["先别动", "不开火", "别开火", "weapons hold", "hold fire"],
        );

    // ── Commander-level (own-scope) mission keywords ──
    let has_point_defense = contains_any(
        &lower,
        &[
            "自卫",
            "自防御",
            "点防御",
            "末端拦截",
            "近防",
            "反蜂群",
            "反小艇",
            "point defense",
            "self-defen",
            "ciws",
            "counter-swarm",
            "hard-kill",
        ],
    );
    let has_interdiction = contains_any(
        &lower,
        &[
            "拦截",
            "封锁",
            "查证",
            "驱离",
            "登临",
            "临检",
            "拦阻",
            "interdict",
            "interdiction",
            "blockade",
            "visit board search",
            "deny passage",
        ],
    );
    let has_escort = contains_any(
        &lower,
        &["护航", "伴随", "护卫", "随行护", "escort", "accompan"],
    );
    let has_picket = contains_any(
        &lower,
        &[
            "哨戒",
            "前出警戒",
            "警戒阵位",
            "预警阵位",
            "picket",
            "early warning",
            "screen station",
        ],
    );
    let has_targeting_handoff = contains_any(
        &lower,
        &[
            "目标交接",
            "制导交接",
            "超视距引导",
            "中继制导",
            "火控交接",
            "targeting handoff",
            "midcourse guidance",
            "over-the-horizon targeting",
            "oth target",
        ],
    );
    let has_deception = contains_any(
        &lower,
        &[
            "欺骗",
            "佯动",
            "诱饵",
            "佯攻",
            "迷惑",
            "示形",
            "deception",
            "decoy",
            "feint",
            "spoof",
        ],
    );

    let (kind, confidence, rationale) = if has_rtb {
        (MissionKind::Rtb, 0.9, "return-to-base keyword")
    } else if has_evade && (has_soft_kill || has_recon_uav_deploy) {
        (
            MissionKind::ReactiveDefense,
            0.9,
            "evasive maneuver + soft-kill/recon-UAV keyword",
        )
    } else if has_recon_uav_deploy {
        (
            MissionKind::Recon,
            0.85,
            "explicit recon-UAV deploy/release keyword",
        )
    } else if has_point_defense {
        (
            MissionKind::PointDefense,
            0.85,
            "self-defense / CIWS / counter-swarm keyword",
        )
    } else if has_interdiction {
        (
            MissionKind::MaritimeInterdiction,
            0.8,
            "maritime interdiction / blockade keyword",
        )
    } else if has_escort {
        (MissionKind::Escort, 0.8, "escort / accompany keyword")
    } else if has_picket {
        (
            MissionKind::Picket,
            0.8,
            "picket / forward screening keyword",
        )
    } else if has_targeting_handoff {
        (
            MissionKind::TargetingHandoff,
            0.8,
            "targeting / midcourse guidance handoff keyword",
        )
    } else if has_deception {
        (
            MissionKind::Deception,
            0.75,
            "deception / decoy / feint keyword",
        )
    } else if (has_track_only || (weapons_hold && !has_strike)) && !has_flank {
        (MissionKind::Track, 0.8, "track-only / weapons-hold keyword")
    } else if has_flank && has_strike {
        (
            MissionKind::ReconFlankStrike,
            0.85,
            "flank + strike keywords → recon-flank-strike",
        )
    } else if has_flank {
        (
            MissionKind::Recon,
            0.8,
            "flank maneuver keyword → recon approach",
        )
    } else if has_coordinated && has_strike {
        (
            MissionKind::CoordinatedStrike,
            0.8,
            "coordinated + strike keywords",
        )
    } else if has_strike {
        (MissionKind::Engage, 0.75, "strike/engage keyword")
    } else if has_recon {
        (MissionKind::Recon, 0.75, "reconnaissance keyword")
    } else if has_patrol {
        (MissionKind::Patrol, 0.7, "patrol keyword")
    } else if intent.maneuver.is_active() {
        (
            MissionKind::Recon,
            0.75,
            "maneuver keyword (heading/speed/flank)",
        )
    } else {
        (MissionKind::Unknown, 0.0, "no tactical keyword matched")
    };

    intent.kind = kind;
    intent.confidence = confidence;
    intent.rationale = rationale.to_string();
    intent
}

/// Classify free-form commander intent text into a [`MissionKind`] using the
/// deterministic keyword layer only (no snapshot grounding required). Used by
/// the slow-loop planner to drive play (style) selection from a
/// [`openfang_types::cognition::CommanderIntent`] objective.
pub fn classify_mission_kind(text: &str) -> MissionKind {
    classify_keyword(text).kind
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

fn parse_explicit_sensor_control(lower: &str) -> Option<(String, String)> {
    let sensor_id = if contains_any(lower, &["雷达", "radar"]) {
        "radar"
    } else if contains_any(lower, &["eoir", "eo/ir", "光电", "红外", "electro-optical"]) {
        "eoir"
    } else if contains_any(lower, &["声呐", "sonar"]) {
        "sonar"
    } else if contains_any(lower, &["激光雷达", "lidar"]) {
        "lidar"
    } else {
        return None;
    };

    let mode = if contains_any(lower, &["关闭", "关", "off", "shut down", "stand down"]) {
        "off"
    } else if contains_any(lower, &["搜索", "扫", "search", "sweep"]) {
        "search"
    } else if contains_any(lower, &["跟踪", "凝视", "track", "gaze"]) {
        "track"
    } else if contains_any(
        lower,
        &["打开", "开启", "启动", "开", "on", "enable", "activate"],
    ) {
        "on"
    } else {
        return None;
    };

    Some((sensor_id.to_string(), mode.to_string()))
}

fn is_sensor_label(label: &str) -> bool {
    let lower = label.to_lowercase();
    contains_any(
        &lower,
        &[
            "雷达",
            "radar",
            "eoir",
            "eo/ir",
            "光电",
            "红外",
            "声呐",
            "sonar",
            "激光雷达",
            "lidar",
        ],
    )
}

fn explicit_recon_uav_deploy(lower: &str) -> bool {
    let has_deploy_verb = contains_any(
        lower,
        &[
            "发射", "释放", "放飞", "部署", "起飞", "launch", "release", "deploy", "send out",
        ],
    );
    let has_recon_uav = contains_any(
        lower,
        &[
            "侦察无人机",
            "侦查无人机",
            "侦察 uav",
            "侦查 uav",
            "scout uav",
            "recon uav",
            "reconnaissance uav",
        ],
    );
    has_deploy_verb && has_recon_uav
}

fn parse_standoff_m(lower: &str) -> Option<f64> {
    // Only treat a distance as standoff when a distance-intent keyword is present.
    let standoff_context = contains_any(
        lower,
        &[
            "安全距离",
            "保持距离",
            "保持安全",
            "standoff",
            "safe distance",
            "保持",
            "距离",
        ],
    );
    if !standoff_context {
        return None;
    }
    let re = regex_lite::Regex::new(
        r"(\d+(?:\.\d+)?)\s*(公里|千米|km|kilometers?|米|metres?|meters?|m)",
    )
    .ok()?;
    let caps = re.captures(lower)?;
    let value: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str();
    let meters = if matches!(unit, "公里" | "千米" | "km") || unit.starts_with("kilom") {
        value * 1000.0
    } else {
        value
    };
    Some(meters)
}

fn parse_patrol_radius_m(lower: &str) -> Option<f64> {
    if !contains_any(lower, &["巡逻", "巡航", "patrol", "cap"]) {
        return None;
    }
    if !contains_any(
        lower,
        &["半径", "圆形", "周边", "周围", "radius", "circular"],
    ) {
        return None;
    }
    parse_distance_m(lower)
}

fn parse_distance_m(lower: &str) -> Option<f64> {
    let re = regex_lite::Regex::new(
        r"(\d+(?:\.\d+)?)\s*(公里|千米|km|kilometers?|海里|nm|nautical miles?|米|metres?|meters?|m)",
    )
    .ok()?;
    let caps = re.captures(lower)?;
    let value: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str();
    let meters = if matches!(unit, "公里" | "千米" | "km") || unit.starts_with("kilom") {
        value * 1000.0
    } else if matches!(unit, "海里" | "nm") || unit.starts_with("nautical") {
        value * 1852.0
    } else {
        value
    };
    Some(meters)
}

fn parse_flank_side(lower: &str) -> Option<FlankSide> {
    if contains_any(
        lower,
        &[
            "左翼",
            "左侧",
            "从左",
            "left flank",
            "left side",
            "从左侧",
            "left approach",
        ],
    ) {
        Some(FlankSide::Left)
    } else if contains_any(
        lower,
        &[
            "右翼",
            "右侧",
            "从右",
            "right flank",
            "right side",
            "从右侧",
            "right approach",
        ],
    ) {
        Some(FlankSide::Right)
    } else {
        None
    }
}

fn parse_maneuver_slots(lower: &str) -> ManeuverIntent {
    let mut m = ManeuverIntent {
        heading_deg: parse_absolute_heading_deg(lower),
        heading_delta_deg: parse_heading_delta_deg(lower),
        turn: parse_turn_hint(lower),
        speed_ms: parse_speed_ms(lower),
        flank_approach: contains_any(
            lower,
            &["绕后", "侧翼", "迂回", "包抄", "flank", "envelop", "pincer"],
        ),
    };
    if m.turn.is_some() && m.heading_deg.is_none() && m.heading_delta_deg.is_none() {
        m.heading_delta_deg = Some(match m.turn {
            Some(FlankSide::Left) => -90.0,
            Some(FlankSide::Right) => 90.0,
            None => 90.0,
        });
    }
    m
}

fn parse_turn_hint(lower: &str) -> Option<FlankSide> {
    if contains_any(
        lower,
        &[
            "左转",
            "向左",
            "向左转",
            "turn left",
            "left turn",
            "port turn",
        ],
    ) {
        Some(FlankSide::Left)
    } else if contains_any(
        lower,
        &[
            "右转",
            "向右",
            "向右转",
            "turn right",
            "right turn",
            "starboard turn",
        ],
    ) {
        Some(FlankSide::Right)
    } else {
        None
    }
}

fn parse_absolute_heading_deg(lower: &str) -> Option<f64> {
    let re = regex_lite::Regex::new(
        r"(?:航向|heading|course|转向)\s*(\d+(?:\.\d+)?)\s*(?:度|°|deg|degrees)?",
    )
    .ok()?;
    let caps = re.captures(lower)?;
    let deg: f64 = caps.get(1)?.as_str().parse().ok()?;
    Some(normalize_heading_deg(deg))
}

fn parse_heading_delta_deg(lower: &str) -> Option<f64> {
    let turn = parse_turn_hint(lower)?;
    let re = regex_lite::Regex::new(
        r"(?:左转|右转|left turn|right turn|turn left|turn right|转)\s*(\d+(?:\.\d+)?)\s*(?:度|°|deg|degrees)?",
    )
    .ok()?;
    let caps = re.captures(lower)?;
    let deg: f64 = caps.get(1)?.as_str().parse().ok()?;
    Some(match turn {
        FlankSide::Left => -deg.abs(),
        FlankSide::Right => deg.abs(),
    })
}

fn parse_speed_ms(lower: &str) -> Option<f64> {
    if contains_any(
        lower,
        &[
            "停止",
            "停船",
            "全停",
            "停下",
            "stop",
            "halt",
            "zero speed",
            "hold position",
        ],
    ) {
        return Some(0.0);
    }
    if contains_any(
        lower,
        &[
            "全速",
            "最快",
            "最大速度",
            "full speed",
            "max speed",
            "flank speed",
        ],
    ) {
        return Some(15.0);
    }
    if let Ok(re) = regex_lite::Regex::new(r"(\d+(?:\.\d+)?)\s*(?:节|knots?|kn)\b") {
        if let Some(caps) = re.captures(lower) {
            if let Ok(knots) = caps.get(1)?.as_str().parse::<f64>() {
                return Some(knots * 0.514444);
            }
        }
    }
    if let Ok(re) = regex_lite::Regex::new(
        r"(?:速度|speed|航速|速率)\s*(\d+(?:\.\d+)?)\s*(米每秒|米/?秒|m/?s|节|knots?|kn)?",
    ) {
        if let Some(caps) = re.captures(lower) {
            if let Ok(v) = caps.get(1)?.as_str().parse::<f64>() {
                let unit = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                return Some(
                    if unit.contains('节') || unit.starts_with("knot") || unit == "kn" {
                        v * 0.514444
                    } else {
                        v
                    },
                );
            }
        }
    }
    if let Ok(re) = regex_lite::Regex::new(r"(\d+(?:\.\d+)?)\s*(?:米每秒|米/?秒|m/?s)\b") {
        if let Some(caps) = re.captures(lower) {
            if let Ok(v) = caps.get(1)?.as_str().parse::<f64>() {
                return Some(v);
            }
        }
    }
    if contains_any(lower, &["加速", "提速", "speed up", "increase speed"]) {
        return Some(10.0);
    }
    if contains_any(lower, &["减速", "慢下来", "slow down", "reduce speed"]) {
        return Some(3.0);
    }
    None
}

fn normalize_heading_deg(deg: f64) -> f64 {
    let mut h = deg % 360.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

fn parse_roe(lower: &str) -> Option<WeaponReleaseLevel> {
    if contains_any(
        lower,
        &[
            "武器先别动",
            "先别动",
            "不开火",
            "别开火",
            "禁止开火",
            "weapons hold",
            "hold fire",
            "只跟踪",
            "仅跟踪",
        ],
    ) {
        Some(WeaponReleaseLevel::WeaponsHold)
    } else if contains_any(
        lower,
        &["自由开火", "自由交战", "weapons free", "fire at will"],
    ) {
        Some(WeaponReleaseLevel::WeaponsFree)
    } else if contains_any(lower, &["谨慎", "weapons tight", "自卫"]) {
        Some(WeaponReleaseLevel::WeaponsTight)
    } else {
        None
    }
}

fn parse_platform_ids(lower: &str) -> Vec<String> {
    let re = regex_lite::Regex::new(r"\b((?:uav|usv|uuv|cca|lsuav|drone)-[a-z0-9_]+)\b");
    let mut ids = Vec::new();
    if let Ok(re) = re {
        for caps in re.captures_iter(lower) {
            if let Some(m) = caps.get(1) {
                let id = m.as_str().to_string();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

fn parse_track_ids(lower: &str) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(re) = regex_lite::Regex::new(r"\b((?:track|trk)-[a-z0-9_]+)\b") {
        for caps in re.captures_iter(lower) {
            if let Some(m) = caps.get(1) {
                let id = m.as_str().to_string();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    // ArkSIM-style explicit track ids embedded in operator text (e.g. "self:1").
    if let Ok(re) = regex_lite::Regex::new(r"\b([a-z][a-z0-9_]*:\d+)\b") {
        for caps in re.captures_iter(lower) {
            if let Some(m) = caps.get(1) {
                let id = m.as_str().to_string();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

fn parse_role_hints(lower: &str) -> Vec<CcaRole> {
    let mut roles = Vec::new();
    if contains_any(
        lower,
        &["侦察", "侦查", "isr", "recon", "scout", "监视", "surveil"],
    ) {
        roles.push(CcaRole::Recon);
    }
    if contains_any(
        lower,
        &[
            "杀伤", "摧毁", "消灭", "歼灭", "察打", "打击", "攻击", "strike", "开火", "fire",
        ],
    ) {
        roles.push(CcaRole::Striker);
    }
    if contains_any(lower, &["干扰", "电子战", "jam", "ew "]) {
        roles.push(CcaRole::EwJamming);
    }
    if contains_any(lower, &["诱饵", "decoy"]) {
        roles.push(CcaRole::Decoy);
    }
    roles
}

/// Pull out semantic target labels: side+type phrases ("蓝方指挥所"). Conservative —
/// only emits a label when a side or recognizable target noun appears, so we do
/// not flood the resolver with noise.
fn parse_target_labels(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut labels = Vec::new();
    let side_words = [
        "蓝方", "红方", "敌方", "敌", "blue", "red", "hostile", "enemy",
    ];
    let target_nouns = [
        "指挥所",
        "指挥部",
        "司令部",
        "command post",
        "headquarters",
        "hq",
        "command",
        "巡逻艇",
        "巡逻舰",
        "巡逻船",
        "patrol boat",
        "patrol vessel",
        "sam_site",
        "sam",
        "radar",
        "防空阵地",
        "雷达",
    ];
    for side in side_words {
        for noun in target_nouns {
            let phrase = format!("{side}{noun}");
            if lower.contains(&phrase) && !labels.contains(&phrase) {
                labels.push(phrase);
            }
        }
    }
    // Whole-phrase fallbacks for common Chinese targets even without a side word.
    for noun in [
        "蓝方指挥所",
        "敌指挥所",
        "敌方指挥所",
        "sam_site",
        "sam site",
        "radar",
        "雷达",
        "防空阵地",
        "sam",
    ] {
        if lower.contains(noun) && !labels.iter().any(|l| l == noun) {
            labels.push(noun.to_string());
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::{ControlledSide, ThreatSide};
    use openfang_types::platform::{
        Affiliation, Domain, FuelStatus, PlatformState, Pose, Track, Velocity, WeaponState,
    };

    fn policy() -> PlatformControlPolicy {
        PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            threat_side: ThreatSide::Opposite,
            controlled_platforms: vec!["red-uav-1".into()],
            own_platform_id: "red-uav-1".into(),
            controller_id: "operator".into(),
            ..Default::default()
        }
    }

    fn snapshot() -> WorldSnapshot {
        let mut own = PlatformState::minimal("red-uav-1");
        own.affiliation = Affiliation::Red;
        own.platform_type = "uav".into();
        own.domain = Domain::Air;
        own.pose = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 1000.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        own.velocity = Velocity {
            speed_ms: 50.0,
            vertical_rate_ms: 0.0,
            course_deg: 0.0,
        };
        own.fuel = FuelStatus {
            remaining_kg: 80.0,
            max_kg: 100.0,
            consumption_rate_kg_s: 0.1,
        };
        own.onboard_weapons = vec![WeaponState {
            weapon_id: "w1".into(),
            weapon_type: "missile".into(),
            quantity_remaining: 2.0,
            max_range_m: Some(20_000.0),
            min_range_m: Some(0.0),
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        }];
        own.tracks = vec![Track {
            track_id: "blue_command_post:1".into(),
            target_name: "blue_command_post".into(),
            classification: "command_post".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((30.05, 120.05, 0.0)),
            heading_deg: None,
            speed_ms: None,
            range_m: Some(5_000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        }];

        let blue = {
            let mut b = PlatformState::minimal("blue_command_post");
            b.affiliation = Affiliation::Blue;
            b.platform_type = "command_post".into();
            b
        };

        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![own, blue],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn llm_task(task_id: &str, action: &str) -> SymbolicTask {
        SymbolicTask {
            task_id: task_id.into(),
            platform: Some("red-uav-1".into()),
            action: action.into(),
            target: None,
            criteria: Some("route_completed".into()),
            preconditions: Vec::new(),
            parameters: serde_json::Map::new(),
            phase: 0,
            ordering: 0,
        }
    }

    #[test]
    fn classifies_recon_flank_strike_with_standoff() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract(
            "绕后使用侦察无人机和察打一体无人机打击蓝方指挥所，注意保持安全距离3公里",
            &snapshot(),
            &policy(),
        );
        assert_eq!(intent.kind, MissionKind::ReconFlankStrike);
        assert_eq!(intent.standoff_m, Some(3000.0));
        assert!(intent.role_hints.contains(&CcaRole::Recon));
        assert!(intent.role_hints.contains(&CcaRole::Striker));
        // "蓝方指挥所" grounded to a real snapshot target.
        assert!(
            intent
                .target_track_ids
                .iter()
                .any(|id| id.starts_with("blue_command_post")),
            "expected grounding to blue command post, got {:?}",
            intent.target_track_ids
        );
    }

    #[test]
    fn classifies_engage_and_grounds_explicit_track() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("打击 track-7", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Engage);
        assert_eq!(intent.target_track_ids, vec!["track-7".to_string()]);
    }

    #[test]
    fn classifies_kill_enemy_patrol_boat_as_engage() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("杀伤敌方巡逻艇", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Engage);
        assert!(
            intent
                .target_labels
                .iter()
                .any(|label| label.contains("巡逻艇")),
            "patrol boat should be extracted as a target label, labels={:?}",
            intent.target_labels
        );
    }

    #[test]
    fn classifies_track_only_and_weapons_hold() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("武器先别动，只跟踪", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Track);
        assert_eq!(intent.roe, Some(WeaponReleaseLevel::WeaponsHold));
    }

    #[test]
    fn classifies_explicit_radar_on_as_sensor_control() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("打开雷达", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::SensorControl);
        assert_eq!(intent.sensor_mode.as_deref(), Some("on"));
        assert_eq!(intent.sensor_id.as_deref(), Some("radar"));
        assert!(
            intent.target_labels.is_empty(),
            "explicit sensor control must not treat radar as target label: {:?}",
            intent.target_labels
        );
    }

    #[test]
    fn classifies_explicit_recon_uav_deploy_as_recon() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("发射侦察无人机侦察 self:1", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Recon);
        assert!(intent
            .target_track_ids
            .iter()
            .any(|track_id| track_id == "self:1"));
    }

    #[test]
    fn classifies_rtb_for_all_platforms() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("所有无人机返航", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Rtb);
        assert!(intent.all_platforms);
        assert!(intent.platform_ids.contains(&"red-uav-1".to_string()));
    }

    #[test]
    fn parses_km_standoff_and_flank_side() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract(
            "uav-2 从左翼侦察 track-7 保持安全距离 2km",
            &snapshot(),
            &policy(),
        );
        assert_eq!(intent.flank_side, Some(FlankSide::Left));
        assert_eq!(intent.standoff_m, Some(2000.0));
    }

    #[test]
    fn parses_circular_patrol_radius() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("巡逻周边100km半径的圆形海域", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Patrol);
        assert_eq!(intent.patrol_radius_m, Some(100_000.0));
        assert_eq!(intent.standoff_m, None);
    }

    #[test]
    fn parses_turn_left_and_speed_maneuver_slots() {
        let extractor = IntentExtractor::new();
        let left = extractor.extract("self 左转", &snapshot(), &policy());
        assert_eq!(left.maneuver.turn, Some(FlankSide::Left));
        assert_eq!(left.maneuver.heading_delta_deg, Some(-90.0));
        assert_eq!(left.kind, MissionKind::Recon);

        let turn20 = extractor.extract("右转20度", &snapshot(), &policy());
        assert_eq!(turn20.maneuver.heading_delta_deg, Some(20.0));

        let speed = extractor.extract("速度8米每秒巡航", &snapshot(), &policy());
        assert_eq!(speed.maneuver.speed_ms, Some(8.0));
    }

    #[test]
    fn parses_flank_maneuver_without_strike_as_recon() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("绕后接近蓝方指挥所", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Recon);
        assert!(intent.maneuver.flank_approach);
    }

    #[test]
    fn unknown_intent_is_low_confidence() {
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("今天天气不错", &snapshot(), &policy());
        assert_eq!(intent.kind, MissionKind::Unknown);
        assert_eq!(intent.confidence, 0.0);
    }

    #[tokio::test]
    async fn llm_layer_upgrades_unknown_intent() {
        struct FakeDriver;
        #[async_trait]
        impl IntentExtractDriver for FakeDriver {
            async fn extract(&self, ctx: ExtractContext) -> Option<StructuredIntent> {
                let mut intent = StructuredIntent::unknown(&ctx.raw_text);
                intent.kind = MissionKind::Recon;
                intent.confidence = 0.7;
                // Reference a real candidate track.
                intent.target_track_ids = ctx.candidate_track_ids.clone();
                intent.task_plan = vec![llm_task("T1", "SensorSetMode")];
                Some(intent)
            }
        }

        let extractor = IntentExtractor::new();
        let intent = extractor
            .extract_with_llm(
                "用诗意的语言描述战场", // no tactical keyword → Unknown
                &snapshot(),
                &policy(),
                Some(&FakeDriver),
                0.5,
            )
            .await;
        assert_eq!(intent.kind, MissionKind::Recon);
        assert_eq!(intent.semantic_source, IntentSemanticSource::Llm);
    }

    #[tokio::test]
    async fn llm_layer_accepts_valid_task_graph_without_keyword_fallback() {
        struct TrackDriver;
        #[async_trait]
        impl IntentExtractDriver for TrackDriver {
            async fn extract(&self, ctx: ExtractContext) -> Option<StructuredIntent> {
                let mut intent = StructuredIntent::unknown(&ctx.raw_text);
                intent.kind = MissionKind::Track;
                intent.confidence = 0.9;
                intent.target_track_ids = vec!["blue_command_post:1".into()];
                intent.task_plan = vec![llm_task("T1", "Designate")];
                Some(intent)
            }
        }

        let extractor = IntentExtractor::new();
        let intent = extractor
            .extract_with_llm(
                "发射侦察无人机侦察蓝方指挥所",
                &snapshot(),
                &policy(),
                Some(&TrackDriver),
                0.5,
            )
            .await;
        assert_eq!(intent.kind, MissionKind::Track);
        assert_eq!(intent.semantic_source, IntentSemanticSource::Llm);
        assert_eq!(intent.fallback_reason, None);
    }

    #[tokio::test]
    async fn llm_layer_rejects_invented_entities() {
        struct HallucinatingDriver;
        #[async_trait]
        impl IntentExtractDriver for HallucinatingDriver {
            async fn extract(&self, _ctx: ExtractContext) -> Option<StructuredIntent> {
                let mut intent = StructuredIntent::unknown("x");
                intent.kind = MissionKind::Engage;
                intent.confidence = 0.9;
                intent.target_track_ids = vec!["ghost-track-999".into()];
                intent.platform_ids = vec!["enemy-uav-1".into()];
                Some(intent)
            }
        }

        let extractor = IntentExtractor::new();
        let intent = extractor
            .extract_with_llm(
                "???",
                &snapshot(),
                &policy(),
                Some(&HallucinatingDriver),
                0.5,
            )
            .await;
        // Invented entities are scrubbed.
        assert!(intent
            .target_track_ids
            .iter()
            .all(|id| id != "ghost-track-999"));
        assert!(intent.platform_ids.iter().all(|id| id != "enemy-uav-1"));
    }

    #[tokio::test]
    async fn llm_layer_runs_before_keyword_even_when_keyword_confident() {
        struct SemanticDriver;
        #[async_trait]
        impl IntentExtractDriver for SemanticDriver {
            async fn extract(&self, _ctx: ExtractContext) -> Option<StructuredIntent> {
                let mut intent = StructuredIntent::unknown("杀伤敌方巡逻艇");
                intent.kind = MissionKind::Engage;
                intent.target_labels = vec!["敌方巡逻艇".into()];
                intent.confidence = 0.91;
                intent.rationale = "kill verb makes this an engagement".into();
                intent.task_plan = vec![llm_task("T1", "Fire")];
                Some(intent)
            }
        }

        let extractor = IntentExtractor::new();
        let intent = extractor
            .extract_with_llm(
                "杀伤敌方巡逻艇",
                &snapshot(),
                &policy(),
                Some(&SemanticDriver),
                0.5,
            )
            .await;
        assert_eq!(intent.kind, MissionKind::Engage);
        assert_eq!(intent.semantic_source, IntentSemanticSource::Llm);
    }

    #[test]
    fn parses_llm_json_object_with_optional_fence_text() {
        let text = r#"
Here is the JSON:
{"kind":"engage","target_labels":["敌方巡逻艇"],"confidence":0.82,"rationale":"semantic strike"}
"#;
        let intent = parse_llm_structured_intent(text, "杀伤敌方巡逻艇").unwrap();
        assert_eq!(intent.kind, MissionKind::Engage);
        assert_eq!(intent.target_labels, vec!["敌方巡逻艇"]);
        assert_eq!(intent.semantic_source, IntentSemanticSource::Llm);
    }
}
