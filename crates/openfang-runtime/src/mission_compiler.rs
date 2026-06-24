//! Mission compiler — `StructuredIntent` + snapshot + Play library → `MissionDsl`.
//!
//! The compiler is fully deterministic: it selects Plays from the [`PlayRegistry`],
//! allocates platforms via a [`PlatformAllocator`] (single-platform by default),
//! binds each Play's function names to concrete [`PlatformCommandSpec`]s bound to
//! the own platform, injects standoff/ROE/PID constraints and human-intervention
//! points, assembles an explanation trace, and runs the mapping validator.
//!
//! Entities are never invented: functions that need a target with none grounded
//! are skipped, lowering confidence rather than fabricating one.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use openfang_types::config::PlatformControlPolicy;
use openfang_types::mission_dsl::{
    Constraint, DslObjective, FunctionCall, InterventionAction, InterventionLevel,
    InterventionPoint, MissionDsl, MissionKind, PlatformCommandSpec, PlayInstance, SafetyGuard,
    ValidationIssue,
};
use openfang_types::platform::{
    Pose, SensorState, SensorType, TurnDirection, Waypoint, WeaponState, WorldSnapshot,
};
use openfang_types::semantic_frame::CommanderFrame;
use openfang_types::umaa::{PlatformLimits, WeaponReleaseLevel};

use crate::flank_geometry::{flank_route, FlankRequest};
use crate::intent_extractor::{
    FlankSide, IntentExtractDriver, IntentExtractor, StructuredIntent, SymbolicTask,
};
use crate::platform_allocator::{AllocationRequest, PlatformAllocator};
use crate::play_registry::{PlayRegistry, PlaySelectionContext};

/// Tunables for one compilation (sourced from `PlanningConfig` at the call site).
#[derive(Debug, Clone)]
pub struct CompileParams {
    pub default_standoff_m: f64,
    pub pid_required: bool,
    pub provenance: String,
    /// Optional home `(lat, lon, alt)` for RTB; falls back to the own pose.
    pub home: Option<(f64, f64, Option<f64>)>,
    /// Cruise speed for generated routes.
    pub speed_ms: Option<f64>,
    /// Hard upper speed used to ground NLP terms such as `speed=max`.
    pub max_speed_ms: f64,
}

impl Default for CompileParams {
    fn default() -> Self {
        Self {
            default_standoff_m: 3000.0,
            pid_required: true,
            provenance: "openfang:mission_compiler".into(),
            home: None,
            speed_ms: Some(50.0),
            max_speed_ms: PlatformLimits::default().max_speed_ms,
        }
    }
}

/// Result of the shared natural-language → structured intent → Mission DSL path.
#[derive(Debug, Clone)]
pub struct ObjectiveCompileOutput {
    pub structured_intent: StructuredIntent,
    pub mission: MissionDsl,
    pub validation_issues: Vec<ValidationIssue>,
}

/// Shared async compiler used by API preview and the kernel slow loop so
/// previewed missions match the missions that can later execute.
pub async fn compile_objective_with_semantics(
    objective: &str,
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
    registry: &PlayRegistry,
    params: &CompileParams,
    driver: Option<&dyn IntentExtractDriver>,
    min_confidence: f64,
) -> ObjectiveCompileOutput {
    let extractor = IntentExtractor::new();
    let structured_intent = extractor
        .extract_with_llm(objective, snapshot, policy, driver, min_confidence)
        .await;
    let symbolic_issues = validate_symbolic_task_plan(&structured_intent, snapshot, policy);
    let compiler = MissionCompiler::new(crate::platform_allocator::SelfPlatformAllocator::new());
    let mission = compiler.compile(&structured_intent, snapshot, registry, policy, params);
    let mut validation_issues = symbolic_issues;
    validation_issues.extend(mission.validate());
    ObjectiveCompileOutput {
        structured_intent,
        mission,
        validation_issues,
    }
}

fn validate_symbolic_task_plan(
    intent: &StructuredIntent,
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
) -> Vec<ValidationIssue> {
    let tasks = &intent.task_plan;
    if tasks.is_empty() {
        return if intent.semantic_source == crate::intent_extractor::IntentSemanticSource::Llm {
            vec![ValidationIssue {
                rule: openfang_types::mission_dsl::ValidationRule::R7,
                message: "llm produced no executable mission_plan.tasks".into(),
            }]
        } else {
            Vec::new()
        };
    }

    let mut issues = Vec::new();
    let mut task_ids = std::collections::HashSet::new();
    let controlled = controlled_platform_ids_for_validation(snapshot, policy);
    let candidates = candidate_track_ids_for_validation(snapshot, policy);

    for task in tasks {
        if !task_ids.insert(task.task_id.clone()) {
            issues.push(symbolic_issue(format!(
                "duplicate symbolic task id '{}'",
                task.task_id
            )));
        }
        let action = normalize_symbolic_action(&task.action);
        if !symbolic_action_allowed(&action) {
            issues.push(symbolic_issue(format!(
                "symbolic task '{}' uses unsupported action '{}'",
                task.task_id, task.action
            )));
        }
        if let Some(platform) = task
            .platform
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let lower = platform.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "self" | "own" | "uav" | "usv" | "uuv" | "heterogeneous"
            ) && !controlled.iter().any(|id| id == platform)
            {
                issues.push(symbolic_issue(format!(
                    "symbolic task '{}' references uncontrolled platform '{}'",
                    task.task_id, platform
                )));
            }
        }
        if task.ordering > 0 && task.preconditions.is_empty() {
            issues.push(symbolic_issue(format!(
                "symbolic task '{}' has ordering {} but no preconditions",
                task.task_id, task.ordering
            )));
        }
        if matches!(action.as_str(), "fire" | "designate") {
            let target = string_param(task, "target_track_id")
                .or_else(|| task.target.clone())
                .unwrap_or_default();
            if target.trim().is_empty() {
                issues.push(symbolic_issue(format!(
                    "symbolic task '{}' action '{}' requires target_track_id",
                    task.task_id, task.action
                )));
            } else if !candidates.iter().any(|candidate| candidate == &target) {
                issues.push(symbolic_issue(format!(
                    "symbolic task '{}' references non-candidate target '{}'",
                    task.task_id, target
                )));
            }
        }
        if action == "sendmessage" {
            let to_platform_id = raw_string_param(task, "to_platform_id").unwrap_or_default();
            let message = raw_string_param(task, "message").unwrap_or_default();
            let known_target_platform = snapshot
                .platforms
                .iter()
                .any(|platform| platform.id == to_platform_id || platform.name == to_platform_id);
            if to_platform_id.is_empty() || !known_target_platform {
                issues.push(symbolic_issue(format!(
                    "symbolic task '{}' action SendMessage requires a known to_platform_id",
                    task.task_id
                )));
            }
            if message.is_empty() {
                issues.push(symbolic_issue(format!(
                    "symbolic task '{}' action SendMessage requires message",
                    task.task_id
                )));
            }
        }
    }

    for task in tasks {
        for precondition in &task.preconditions {
            if let Some(dep) = precondition.strip_suffix("_complete") {
                if !task_ids.contains(dep) {
                    issues.push(symbolic_issue(format!(
                        "symbolic task '{}' references unknown precondition '{}'",
                        task.task_id, precondition
                    )));
                }
            } else if !(precondition.starts_with("event:") || precondition.starts_with("feedback:"))
            {
                issues.push(symbolic_issue(format!(
                    "symbolic task '{}' has invalid precondition '{}'",
                    task.task_id, precondition
                )));
            }
        }
    }

    if symbolic_task_graph_has_cycle(tasks) {
        issues.push(symbolic_issue(
            "symbolic task dependency graph contains a cycle".to_string(),
        ));
    }
    issues.extend(validate_symbolic_frontier_lanes(tasks));
    issues
}

fn symbolic_issue(message: String) -> ValidationIssue {
    ValidationIssue {
        rule: openfang_types::mission_dsl::ValidationRule::R7,
        message,
    }
}

fn symbolic_action_allowed(action: &str) -> bool {
    matches!(
        action,
        "followroute"
            | "goto"
            | "setheading"
            | "setspeed"
            | "sensoron"
            | "sensoroff"
            | "sensorsetmode"
            | "designate"
            | "fire"
            | "jam"
            | "jamstop"
            | "sendmessage"
    )
}

fn symbolic_task_graph_has_cycle(tasks: &[SymbolicTask]) -> bool {
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
            return true;
        }
        if matches!(visits.get(id), Some(Visit::Done)) {
            return false;
        }
        visits.insert(id, Visit::Visiting);
        if let Some(deps) = edges.get(id) {
            for dep in deps {
                if visit(dep, edges, visits) {
                    return true;
                }
            }
        }
        visits.insert(id, Visit::Done);
        false
    }

    let mut edges: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for task in tasks {
        let deps = task
            .preconditions
            .iter()
            .filter_map(|precondition| precondition.strip_suffix("_complete"))
            .collect::<Vec<_>>();
        edges.insert(task.task_id.as_str(), deps);
    }
    let mut visits = std::collections::HashMap::new();
    edges.keys().any(|id| visit(id, &edges, &mut visits))
}

fn validate_symbolic_frontier_lanes(tasks: &[SymbolicTask]) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let mut lanes: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for task in tasks {
        let action = normalize_symbolic_action(&task.action);
        let Some(lane) = symbolic_action_lane(&action) else {
            continue;
        };
        let mut preconditions = task.preconditions.clone();
        preconditions.sort();
        let platform = task.platform.as_deref().unwrap_or("self");
        let key = format!("{platform}:{lane}:{preconditions:?}");
        if let Some(existing) = lanes.insert(key, task.task_id.as_str()) {
            issues.push(symbolic_issue(format!(
                "symbolic tasks '{}' and '{}' may concurrently contend for {} lane",
                existing, task.task_id, lane
            )));
        }
    }
    issues
}

fn symbolic_action_lane(action: &str) -> Option<&'static str> {
    match action {
        "followroute" | "goto" | "setheading" | "setspeed" => Some("motion"),
        "sensoron" | "sensoroff" | "sensorsetmode" => Some("sensor"),
        "designate" | "fire" => Some("weapon"),
        "jam" | "jamstop" => Some("electronic_warfare"),
        "sendmessage" => Some("comm"),
        _ => None,
    }
}

fn controlled_platform_ids_for_validation(
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
) -> Vec<String> {
    let mut ids = if policy.controlled_platforms.is_empty() {
        snapshot
            .platforms
            .iter()
            .filter(|platform| policy.controlled_side.matches(platform.affiliation))
            .map(|platform| platform.id.clone())
            .collect::<Vec<_>>()
    } else {
        policy.controlled_platforms.clone()
    };
    if !policy.own_platform_id.is_empty() && !ids.contains(&policy.own_platform_id) {
        ids.push(policy.own_platform_id.clone());
    }
    ids
}

fn candidate_track_ids_for_validation(
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
) -> Vec<String> {
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

/// Stateless mission compiler bound to an allocator.
pub struct MissionCompiler<A: PlatformAllocator> {
    allocator: A,
}

impl<A: PlatformAllocator> MissionCompiler<A> {
    pub fn new(allocator: A) -> Self {
        Self { allocator }
    }

    /// Compile a Tier-1 semantic frame. The legacy `StructuredIntent` remains
    /// the compatibility view used by existing callers and tests.
    pub fn compile_frame(
        &self,
        frame: &CommanderFrame,
        snapshot: &WorldSnapshot,
        registry: &PlayRegistry,
        policy: &PlatformControlPolicy,
        params: &CompileParams,
    ) -> MissionDsl {
        let intent: StructuredIntent = frame.into();
        self.compile(&intent, snapshot, registry, policy, params)
    }

    /// Compile a grounded structured intent into an auditable `MissionDsl`.
    pub fn compile(
        &self,
        intent: &StructuredIntent,
        snapshot: &WorldSnapshot,
        registry: &PlayRegistry,
        policy: &PlatformControlPolicy,
        params: &CompileParams,
    ) -> MissionDsl {
        let own_platform_id = if policy.own_platform_id.is_empty() {
            "self"
        } else {
            policy.own_platform_id.as_str()
        };

        let standoff_m = intent.standoff_m.unwrap_or(params.default_standoff_m);
        let salvo_size = parse_salvo_size(&intent.raw_text);
        // Resolve EVERY requested target to a REAL firing track id held in the
        // snapshot, de-duplicated and kept in the operator's priority order. A
        // plural intent ("strike the blue surface vessels") must engage every
        // grounded target, not just the first one the LLM happened to list.
        // The LLM frequently picks the human-readable enemy name (e.g.
        // "blue_patrol_3") over the cryptic track id ("self:3"); firing at a
        // bare name makes AFSIM log `TrackID is error:...` and null-deref.
        // Unresolvable targets are dropped (no real track ⇒ cannot fire),
        // never fabricated.
        let mut resolved_targets: Vec<String> = Vec::new();
        for raw in &intent.target_track_ids {
            if let Some(tid) = resolve_track_id(snapshot, raw) {
                if !resolved_targets.contains(&tid) {
                    resolved_targets.push(tid);
                }
            }
        }
        // The primary target drives pose-relative geometry (flank route, MMS
        // standoff), play selection and objectives — one mission, one anchor.
        let target_track_id = resolved_targets.first().cloned();
        let own = snapshot.find_platform(own_platform_id);
        let (weapon_id, has_weapon) = own
            .and_then(|p| select_recommended_weapon(&p.onboard_weapons))
            // No known weapon → empty id + has_weapon=false. A fabricated name
            // ("primary") points at a non-existent weapon part and crashes
            // simulators (AFSIM null-derefs in WsfPlatformPartEvent::Execute).
            .unwrap_or_else(|| (String::new(), false));
        // Inventory ceiling: never schedule more fires than rounds on hand.
        // Each target consumes one salvo (default 1 round); targets beyond the
        // ceiling are kept for annotation, never silently engaged. Truncation
        // follows priority order (operator's `priority_tracks`/`labels` first).
        let salvo_per = f64::from(salvo_size.unwrap_or(1).max(1));
        let weapon_qty = own
            .and_then(|p| p.onboard_weapons.iter().find(|w| w.weapon_id == weapon_id))
            .map(|w| w.quantity_remaining)
            .unwrap_or(0.0);
        let max_engageable = if has_weapon && salvo_per > 0.0 {
            (weapon_qty / salvo_per).floor() as usize
        } else {
            0
        };
        let engaged_count = resolved_targets.len().min(max_engageable);
        let fire_targets: Vec<String> = resolved_targets[..engaged_count].to_vec();
        let unengaged_targets: Vec<String> = resolved_targets[engaged_count..].to_vec();
        // Designation is non-lethal: an ISR/track mission with no weapon still
        // designates its targets. Fall back to the full resolved set when no
        // fire is scheduled so designation is never gated on weapon inventory.
        let designate_targets: Vec<String> = if fire_targets.is_empty() {
            resolved_targets.clone()
        } else {
            fire_targets.clone()
        };
        let scout_uav_weapon_id = own.and_then(|p| select_scout_uav_weapon(&p.onboard_weapons));
        let decoy_weapon_id = own.and_then(|p| select_decoy_weapon(&p.onboard_weapons));
        let jammer_id = own.and_then(|p| {
            p.onboard_jammers
                .iter()
                .find(|jammer| !jammer.jammer_id.trim().is_empty())
                .map(|jammer| jammer.jammer_id.clone())
        });
        // Sensor component id: use the platform's real first sensor when known,
        // otherwise an EMPTY id. Empty = the validated "all/default sensors"
        // path (matches warlock_command_walkthrough turn_on_sensor(agent, "")).
        // A made-up "primary" sensor does not exist on most platforms and
        // crashes AFSIM when the plugin schedules a part event on a null sensor.
        let sensor_id = select_sensor_id(
            own.map(|p| p.onboard_sensors.as_slice()).unwrap_or(&[]),
            intent.sensor_id.as_deref(),
        )
        .unwrap_or_default();
        let has_sensor = own.map(|p| !p.onboard_sensors.is_empty()).unwrap_or(true);

        let select_ctx = PlaySelectionContext {
            has_weapon,
            has_sensor,
            has_target: target_track_id.is_some() || intent.kind == MissionKind::ReactiveDefense,
            pid_or_designated: target_track_id.is_some(),
            controlled_platform_count: intent.platform_ids.len().max(1),
            ..PlaySelectionContext::new()
        };
        let selected = registry.select(intent.kind, &select_ctx);

        let fn_ctx = FnCtx {
            own_platform_id,
            target_track_id: target_track_id.as_deref(),
            fire_targets: &fire_targets,
            designate_targets: &designate_targets,
            target_pose: target_track_id
                .as_deref()
                .and_then(|id| resolve_target_pose(snapshot, id)),
            own_pose: own.map(|p| p.pose),
            standoff_m,
            patrol_radius_m: intent.patrol_radius_m,
            flank_side: intent.flank_side,
            weapon_id,
            scout_uav_weapon_id,
            decoy_weapon_id,
            jammer_id,
            has_weapon,
            sensor_id,
            sensor_mode: intent.sensor_mode.clone(),
            speed_ms: params.speed_ms,
            max_speed_ms: params.max_speed_ms,
            salvo_size,
            home: params.home,
            timestamp: snapshot.timestamp,
            snapshot,
            policy,
        };

        let mut plays: Vec<PlayInstance> = Vec::new();
        let mut functions: Vec<FunctionCall> = Vec::new();

        if !intent.task_plan.is_empty() {
            if validate_symbolic_task_plan(intent, snapshot, policy).is_empty() {
                plays.push(PlayInstance {
                    play_id: "SymbolicPlan".into(),
                    assigned_platforms: intent
                        .platform_ids
                        .clone()
                        .into_iter()
                        .chain(std::iter::once(own_platform_id.to_string()))
                        .collect(),
                    role: openfang_types::platform::CcaRole::Adaptive,
                    phase: 0,
                });
                functions.extend(build_symbolic_task_functions(&intent.task_plan, &fn_ctx));
            }
        } else {
            for play in &selected {
                let assignments = self.allocator.allocate(&AllocationRequest {
                    play,
                    snapshot,
                    own_platform_id,
                    explicit_platforms: &intent.platform_ids,
                });
                for assignment in &assignments {
                    plays.push(PlayInstance {
                        play_id: play.name.clone(),
                        assigned_platforms: vec![assignment.platform_id.clone()],
                        role: assignment.role,
                        phase: assignment.phase,
                    });
                }
                for (idx, fname) in play.functions.iter().enumerate() {
                    for (assignment_idx, assignment) in assignments.iter().enumerate() {
                        functions.extend(build_functions(
                            fname,
                            &play.name,
                            idx,
                            assignment_idx,
                            &assignment.platform_id,
                            &fn_ctx,
                        ));
                    }
                }
            }
        }

        if intent.kind == MissionKind::SensorControl {
            functions.extend(build_sensor_control_function(&fn_ctx));
        }

        inject_maneuver_functions(intent, &fn_ctx, &mut functions);

        // The extractor's classification is authoritative for `kind`. An empty
        // play selection (e.g. a lethal intent with no grounded target) does not
        // reclassify the mission — it lowers confidence and yields no functions,
        // signalling "understood, but needs clarification" to the operator.
        let kind = intent.kind;

        let has_lethal = functions.iter().any(FunctionCall::is_lethal);
        let constraints =
            build_constraints(intent.roe, standoff_m, params.pid_required, has_lethal);
        let intervention_points = build_intervention_points(has_lethal);
        let objectives = build_objectives(kind, target_track_id.as_deref(), own_platform_id);
        let mut explanation_trace = build_explanation_trace(kind, &plays, &functions);
        if !unengaged_targets.is_empty() {
            explanation_trace.push_str(&format!(
                " | 因弹药不足未交战 ({} 发可用 / 每目标 {} 发): {}",
                weapon_qty as u64,
                salvo_per as u64,
                unengaged_targets.join(", ")
            ));
        }

        // Confidence: start from extraction confidence, degrade when no play could
        // be bound, and penalize a lethal intent that could not bind a target.
        let mut confidence = intent.confidence;
        if kind == MissionKind::Unknown {
            confidence = 0.0;
        } else if selected.is_empty() && kind != MissionKind::SensorControl {
            confidence *= 0.3;
        } else if kind.is_lethal_class() && target_track_id.is_none() {
            confidence *= 0.5;
        }

        MissionDsl {
            id: mission_id(&intent.raw_text, snapshot.timestamp),
            intent_text: intent.raw_text.clone(),
            kind,
            time_window: None,
            objectives,
            constraints,
            plays,
            functions,
            intervention_points,
            explanation_trace,
            confidence,
            provenance: params.provenance.clone(),
        }
    }
}

/// Validate a compiled mission (delegates to the DSL validator).
pub fn validate(mission: &MissionDsl) -> Vec<ValidationIssue> {
    mission.validate()
}

fn select_recommended_weapon(weapons: &[WeaponState]) -> Option<(String, bool)> {
    weapons
        .iter()
        .filter(|w| w.is_ready && w.quantity_remaining > 0.0)
        .max_by_key(|w| recommended_weapon_rank(&w.weapon_id, &w.weapon_type))
        .map(|w| (w.weapon_id.clone(), true))
}

fn select_scout_uav_weapon(weapons: &[WeaponState]) -> Option<String> {
    weapons
        .iter()
        .filter(|w| w.is_ready && w.quantity_remaining > 0.0)
        .find(|w| {
            let text = format!("{} {}", w.weapon_id, w.weapon_type).to_ascii_lowercase();
            openfang_types::mission_dsl::is_recon_uav_weapon_id(&text)
        })
        .map(|w| w.weapon_id.clone())
}

fn select_decoy_weapon(weapons: &[WeaponState]) -> Option<String> {
    weapons
        .iter()
        .filter(|w| w.is_ready && w.quantity_remaining > 0.0)
        .find(|w| {
            let text = format!("{} {}", w.weapon_id, w.weapon_type).to_ascii_lowercase();
            text.contains("chaff")
                || text.contains("decoy")
                || text.contains("countermeasure")
                || text.contains("flare")
        })
        .map(|w| w.weapon_id.clone())
}

fn select_sensor_id(sensors: &[SensorState], hint: Option<&str>) -> Option<String> {
    let hint = hint.map(str::trim).filter(|h| !h.is_empty());
    if let Some(hint) = hint {
        if let Some(sensor) = sensors.iter().find(|sensor| sensor.sensor_id == hint) {
            return Some(sensor.sensor_id.clone());
        }
        if let Some(sensor_type) = sensor_type_for_hint(hint) {
            if let Some(sensor) = sensors
                .iter()
                .find(|sensor| sensor.sensor_type == sensor_type)
            {
                return Some(sensor.sensor_id.clone());
            }
        }
    }
    sensors.first().map(|sensor| sensor.sensor_id.clone())
}

fn sensor_type_for_hint(hint: &str) -> Option<SensorType> {
    let lower = hint.to_lowercase();
    if lower.contains("radar") || lower.contains("雷达") {
        Some(SensorType::Radar)
    } else if lower.contains("eoir") || lower.contains("eo/ir") || lower.contains("光电") {
        Some(SensorType::EOIR)
    } else if lower.contains("sonar") || lower.contains("声呐") {
        Some(SensorType::Sonar)
    } else if lower.contains("lidar") || lower.contains("激光雷达") {
        Some(SensorType::Lidar)
    } else if lower.contains("esm") {
        Some(SensorType::ESM)
    } else if lower.contains("ais") {
        Some(SensorType::AIS)
    } else {
        None
    }
}

fn recommended_weapon_rank(weapon_id: &str, weapon_type: &str) -> u16 {
    let text = format!("{} {}", weapon_id, weapon_type).to_ascii_lowercase();
    let base = if text.contains("loiter") || text.contains("munition") || text.contains("mun") {
        500
    } else if text.contains("uav") {
        400
    } else if text.contains("missile") || text.contains("rocket") || text.contains("torpedo") {
        300
    } else if text.contains("gun") || text.contains("bullet") || text.contains("cannon") {
        200
    } else if text.contains("loiter") || text.contains("munition") {
        100
    } else {
        0
    };
    base + trailing_number(weapon_id).unwrap_or(0).min(99)
}

fn trailing_number(text: &str) -> Option<u16> {
    let digits: String = text
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

fn parse_salvo_size(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("single fire") || text.contains("单发") {
        return Some(1);
    }
    for marker in ["salvo_size=", "salvo-size=", "salvo size", "salvo", "齐射"] {
        if let Some(size) = parse_u32_after_marker(text, marker) {
            return Some(size.clamp(1, 8));
        }
    }
    if text.contains("两枚") || text.contains("两发") || text.contains("双发") {
        return Some(2);
    }
    None
}

fn parse_u32_after_marker(text: &str, marker: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find(marker)? + marker.len();
    let tail = &text[start..];
    let digits: String = tail
        .chars()
        .skip_while(|c| c.is_whitespace() || matches!(c, ':' | '=' | '：' | '，' | ',' | '-' | '_'))
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

// ─────────────────────────────────────────────
// Function binding
// ─────────────────────────────────────────────

struct FnCtx<'a> {
    own_platform_id: &'a str,
    target_track_id: Option<&'a str>,
    /// Inventory-capped, priority-ordered targets to FIRE at (lethal).
    fire_targets: &'a [String],
    /// Targets to DESIGNATE (non-lethal); equals the engaged set, or the full
    /// resolved set when no fire is scheduled (ISR/track missions).
    designate_targets: &'a [String],
    target_pose: Option<TargetPose>,
    own_pose: Option<Pose>,
    standoff_m: f64,
    patrol_radius_m: Option<f64>,
    flank_side: Option<FlankSide>,
    weapon_id: String,
    scout_uav_weapon_id: Option<String>,
    decoy_weapon_id: Option<String>,
    jammer_id: Option<String>,
    has_weapon: bool,
    sensor_id: String,
    sensor_mode: Option<String>,
    speed_ms: Option<f64>,
    max_speed_ms: f64,
    salvo_size: Option<u32>,
    home: Option<(f64, f64, Option<f64>)>,
    timestamp: f64,
    snapshot: &'a WorldSnapshot,
    policy: &'a PlatformControlPolicy,
}

#[derive(Debug, Clone, Copy)]
struct TargetPose {
    lat: f64,
    lon: f64,
    alt: Option<f64>,
    heading_deg: Option<f64>,
}

fn lethal_safety_guard() -> SafetyGuard {
    SafetyGuard {
        preconditions: vec![
            "positive_identification".into(),
            "target_in_engagement_zone".into(),
        ],
        abort_rules: vec!["target_lost".into(), "collateral_risk_exceeded".into()],
        lme_checklist: vec!["no_strike_list_clear".into(), "roe_permits_release".into()],
    }
}

fn build_sensor_control_function(ctx: &FnCtx<'_>) -> Vec<FunctionCall> {
    let Some(mode) = ctx.sensor_mode.as_deref() else {
        return Vec::new();
    };
    let command = match mode {
        "on" => PlatformCommandSpec::SensorOn {
            sensor_id: ctx.sensor_id.clone(),
        },
        "off" => PlatformCommandSpec::SensorOff {
            sensor_id: ctx.sensor_id.clone(),
        },
        other => PlatformCommandSpec::SensorSetMode {
            sensor_id: ctx.sensor_id.clone(),
            mode: other.to_string(),
        },
    };
    vec![FunctionCall {
        id: format!("sensor_control:{}:{mode}", ctx.own_platform_id),
        task_id: format!("sensor_control:{}:{mode}", ctx.own_platform_id),
        parent_play: "SensorControl".into(),
        platform_id: ctx.own_platform_id.to_string(),
        command,
        preconditions: Vec::new(),
        criteria: None,
        phase: 0,
        ordering: 0,
        service: None,
        safety_guard: SafetyGuard::default(),
    }]
}

fn build_symbolic_task_functions(tasks: &[SymbolicTask], ctx: &FnCtx<'_>) -> Vec<FunctionCall> {
    tasks
        .iter()
        .filter_map(|task| {
            let action = normalize_symbolic_action(&task.action);
            let platform_id = symbolic_platform_id(task, ctx);
            let speed_ms = symbolic_speed_ms(task, ctx);
            let command = match action.as_str() {
                "followroute" => symbolic_follow_route(task, &platform_id, ctx, speed_ms),
                "goto" => symbolic_goto(task, &platform_id, ctx, speed_ms),
                "setheading" => Some(PlatformCommandSpec::SetHeading {
                    heading_deg: numeric_param(task, "heading_deg")?,
                    speed_ms,
                    turn_direction: string_param(task, "turn").and_then(|turn| {
                        match turn.as_str() {
                            "left" => Some(TurnDirection::Left),
                            "right" => Some(TurnDirection::Right),
                            _ => None,
                        }
                    }),
                }),
                "setspeed" => speed_ms.map(|speed_ms| PlatformCommandSpec::SetSpeed { speed_ms }),
                "sensoron" => Some(PlatformCommandSpec::SensorOn {
                    sensor_id: symbolic_sensor_id(task, &platform_id, ctx),
                }),
                "sensoroff" => Some(PlatformCommandSpec::SensorOff {
                    sensor_id: symbolic_sensor_id(task, &platform_id, ctx),
                }),
                "sensorsetmode" => Some(PlatformCommandSpec::SensorSetMode {
                    sensor_id: symbolic_sensor_id(task, &platform_id, ctx),
                    mode: string_param(task, "mode").unwrap_or_else(|| "search".into()),
                }),
                "jam" => ctx
                    .jammer_id
                    .as_ref()
                    .map(|jammer_id| PlatformCommandSpec::Jam {
                        jammer_id: jammer_id.clone(),
                        technique: string_param(task, "technique")
                            .or_else(|| Some("self_protection".into())),
                        frequency_hz: numeric_param(task, "frequency_hz"),
                        bandwidth_hz: numeric_param(task, "bandwidth_hz"),
                        target_track_id: symbolic_target_track_id(task, ctx),
                    }),
                "jamstop" => ctx
                    .jammer_id
                    .as_ref()
                    .map(|jammer_id| PlatformCommandSpec::JamStop {
                        jammer_id: jammer_id.clone(),
                    }),
                "sendmessage" => Some(PlatformCommandSpec::SendMessage {
                    to_platform_id: raw_string_param(task, "to_platform_id")?,
                    message: raw_string_param(task, "message")?,
                }),
                "designate" => symbolic_target_track_id(task, ctx)
                    .map(|track_id| PlatformCommandSpec::Designate { track_id }),
                "fire" => {
                    let track_id = symbolic_target_track_id(task, ctx)?;
                    if ctx.has_weapon {
                        Some(PlatformCommandSpec::Fire {
                            weapon_id: ctx.weapon_id.clone(),
                            track_id,
                            salvo_size: u32_param(task, "salvo_size").or(ctx.salvo_size),
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            }?;
            let is_lethal = command.is_lethal();
            Some(FunctionCall {
                id: format!("symbolic:{}:{}", platform_id, task.task_id),
                task_id: task.task_id.clone(),
                parent_play: "SymbolicPlan".into(),
                platform_id,
                command,
                preconditions: task.preconditions.clone(),
                criteria: task.criteria.clone(),
                phase: task.phase,
                ordering: task.ordering,
                service: None,
                safety_guard: if is_lethal {
                    lethal_safety_guard()
                } else {
                    SafetyGuard::default()
                },
            })
        })
        .collect()
}

fn symbolic_platform_id(task: &SymbolicTask, ctx: &FnCtx<'_>) -> String {
    let Some(platform) = task
        .platform
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        return ctx.own_platform_id.to_string();
    };
    let lower = platform.to_ascii_lowercase();
    if matches!(lower.as_str(), "self" | "own" | "heterogeneous") {
        return ctx.own_platform_id.to_string();
    }
    if let Some(platform_state) = ctx
        .snapshot
        .platforms
        .iter()
        .filter(|candidate| symbolic_platform_allowed(candidate, ctx))
        .find(|candidate| candidate.id == platform || candidate.name == platform)
    {
        return platform_state.id.clone();
    }
    if matches!(lower.as_str(), "uav" | "usv" | "uuv") {
        if let Some(platform_state) = ctx
            .snapshot
            .platforms
            .iter()
            .filter(|candidate| symbolic_platform_allowed(candidate, ctx))
            .find(|candidate| {
                candidate
                    .platform_type
                    .to_ascii_lowercase()
                    .contains(lower.as_str())
            })
        {
            return platform_state.id.clone();
        }
    }
    ctx.own_platform_id.to_string()
}

fn symbolic_platform_allowed(
    platform: &openfang_types::platform::PlatformState,
    ctx: &FnCtx<'_>,
) -> bool {
    if platform.id == ctx.own_platform_id {
        return true;
    }
    if !ctx.policy.controlled_platforms.is_empty()
        && !ctx.policy.controlled_platforms.contains(&platform.id)
    {
        return false;
    }
    ctx.policy.controlled_side.matches(platform.affiliation)
}

fn normalize_symbolic_action(action: &str) -> String {
    action
        .chars()
        .filter(|ch| !matches!(ch, '_' | '-' | ' '))
        .collect::<String>()
        .to_ascii_lowercase()
}

fn string_param(task: &SymbolicTask, key: &str) -> Option<String> {
    task.parameters
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
}

fn raw_string_param(task: &SymbolicTask, key: &str) -> Option<String> {
    task.parameters
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn numeric_param(task: &SymbolicTask, key: &str) -> Option<f64> {
    task.parameters.get(key).and_then(|value| match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    })
}

fn u32_param(task: &SymbolicTask, key: &str) -> Option<u32> {
    numeric_param(task, key).map(|value| value.max(0.0) as u32)
}

fn symbolic_target_track_id(task: &SymbolicTask, ctx: &FnCtx<'_>) -> Option<String> {
    string_param(task, "target_track_id")
        .or_else(|| task.target.as_ref().map(|target| target.trim().to_string()))
        .filter(|target| !target.is_empty())
        .or_else(|| ctx.target_track_id.map(str::to_string))
}

fn symbolic_platform_pose(platform_id: &str, ctx: &FnCtx<'_>) -> Option<Pose> {
    ctx.snapshot
        .platforms
        .iter()
        .find(|platform| platform.id == platform_id)
        .map(|platform| platform.pose)
        .or(ctx.own_pose)
}

fn symbolic_sensor_id(task: &SymbolicTask, platform_id: &str, ctx: &FnCtx<'_>) -> String {
    let hint = string_param(task, "sensor");
    ctx.snapshot
        .platforms
        .iter()
        .find(|platform| platform.id == platform_id)
        .and_then(|platform| select_sensor_id(&platform.onboard_sensors, hint.as_deref()))
        .unwrap_or_else(|| ctx.sensor_id.clone())
}

fn symbolic_speed_ms(task: &SymbolicTask, ctx: &FnCtx<'_>) -> Option<f64> {
    match task.parameters.get("speed") {
        Some(serde_json::Value::String(s)) if s.eq_ignore_ascii_case("max") => {
            Some(ctx.max_speed_ms)
        }
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|speed| speed.min(ctx.max_speed_ms)),
        _ => ctx.speed_ms.map(|speed| speed.min(ctx.max_speed_ms)),
    }
}

fn symbolic_follow_route(
    task: &SymbolicTask,
    platform_id: &str,
    ctx: &FnCtx<'_>,
    speed_ms: Option<f64>,
) -> Option<PlatformCommandSpec> {
    match string_param(task, "route_shape").as_deref() {
        Some("circle") => {
            let center = symbolic_platform_pose(platform_id, ctx)?;
            let radius_m = numeric_param(task, "radius_m").or(ctx.patrol_radius_m)?;
            Some(PlatformCommandSpec::FollowRoute {
                waypoints: circular_patrol_route(center, radius_m, speed_ms),
            })
        }
        Some("polyline") => {
            let waypoints = task
                .parameters
                .get("waypoints")?
                .as_array()?
                .iter()
                .filter_map(|value| {
                    Some(Waypoint {
                        lat: value.get("lat")?.as_f64()?,
                        lon: value.get("lon")?.as_f64()?,
                        alt: value.get("alt").and_then(|alt| alt.as_f64()),
                        speed_ms,
                    })
                })
                .collect::<Vec<_>>();
            (!waypoints.is_empty()).then_some(PlatformCommandSpec::FollowRoute { waypoints })
        }
        _ => None,
    }
}

fn symbolic_goto(
    task: &SymbolicTask,
    platform_id: &str,
    ctx: &FnCtx<'_>,
    speed_ms: Option<f64>,
) -> Option<PlatformCommandSpec> {
    if let (Some(lat), Some(lon)) = (numeric_param(task, "lat"), numeric_param(task, "lon")) {
        return Some(PlatformCommandSpec::Goto {
            lat,
            lon,
            alt: numeric_param(task, "alt"),
            speed_ms,
        });
    }
    if let Some(target_id) = symbolic_target_track_id(task, ctx) {
        if let Some(target) = resolve_target_pose(ctx.snapshot, &target_id) {
            return Some(PlatformCommandSpec::Goto {
                lat: target.lat,
                lon: target.lon,
                alt: target.alt,
                speed_ms,
            });
        }
    }
    if let Some(target) = ctx.target_pose {
        return Some(PlatformCommandSpec::Goto {
            lat: target.lat,
            lon: target.lon,
            alt: target.alt,
            speed_ms,
        });
    }
    let own = symbolic_platform_pose(platform_id, ctx)?;
    let (lat, lon) =
        crate::flank_geometry::destination(own.lat_deg, own.lon_deg, own.heading_deg, 1000.0);
    Some(PlatformCommandSpec::Goto {
        lat,
        lon,
        alt: Some(own.alt_m),
        speed_ms,
    })
}

fn circular_patrol_route(center: Pose, radius_m: f64, speed_ms: Option<f64>) -> Vec<Waypoint> {
    const SEGMENTS: usize = 8;
    let radius_m = radius_m.clamp(100.0, 500_000.0);
    let mut waypoints = Vec::with_capacity(SEGMENTS + 1);
    for idx in 0..SEGMENTS {
        let bearing_deg =
            normalize_heading_deg(center.heading_deg + idx as f64 * 360.0 / SEGMENTS as f64);
        let (lat, lon) = crate::flank_geometry::destination(
            center.lat_deg,
            center.lon_deg,
            bearing_deg,
            radius_m,
        );
        waypoints.push(Waypoint {
            lat,
            lon,
            alt: Some(center.alt_m),
            speed_ms,
        });
    }
    if let Some(first) = waypoints.first().cloned() {
        waypoints.push(first);
    }
    waypoints
}

/// Build the concrete function calls for a single play function name.
fn build_functions(
    fname: &str,
    play_id: &str,
    idx: usize,
    assignment_idx: usize,
    platform_id: &str,
    ctx: &FnCtx<'_>,
) -> Vec<FunctionCall> {
    let mk = |suffix: &str, command: PlatformCommandSpec, lethal: bool| {
        let id = format!("{play_id}:{platform_id}:{assignment_idx}:{idx}:{fname}:{suffix}");
        FunctionCall {
            id: id.clone(),
            task_id: id,
            parent_play: play_id.to_string(),
            platform_id: platform_id.to_string(),
            command,
            preconditions: Vec::new(),
            criteria: None,
            phase: idx as u32,
            ordering: assignment_idx as u32,
            service: None,
            safety_guard: if lethal {
                lethal_safety_guard()
            } else {
                SafetyGuard::default()
            },
        }
    };

    match fname {
        "evasive_maneuver" => {
            let Some(own) = ctx.own_pose else {
                return Vec::new();
            };
            let mut call = mk(
                "evade",
                PlatformCommandSpec::SetHeading {
                    heading_deg: normalize_heading_deg(own.heading_deg + 90.0),
                    speed_ms: ctx.speed_ms.or(Some(80.0)),
                    turn_direction: Some(TurnDirection::Right),
                },
                false,
            );
            call.preconditions = vec!["event:missile_inbound".into()];
            call.criteria = Some("missile_miss_distance_safe".into());
            vec![call]
        }
        "release_decoy" => ctx
            .decoy_weapon_id
            .as_ref()
            .map(|weapon_id| {
                let mut call = mk(
                    "decoy",
                    PlatformCommandSpec::ReleaseDecoy {
                        weapon_id: weapon_id.clone(),
                        count: 1,
                        interval_s: 0.25,
                    },
                    false,
                );
                call.preconditions = vec!["event:missile_inbound".into()];
                call.criteria = Some("decoy_released".into());
                vec![call]
            })
            .unwrap_or_default(),
        "start_jam" => ctx
            .jammer_id
            .as_ref()
            .map(|jammer_id| {
                let mut call = mk(
                    "jam",
                    PlatformCommandSpec::Jam {
                        jammer_id: jammer_id.clone(),
                        technique: Some("self_protection".into()),
                        frequency_hz: None,
                        bandwidth_hz: None,
                        target_track_id: ctx.target_track_id.map(str::to_string),
                    },
                    false,
                );
                call.preconditions = vec!["event:missile_inbound".into()];
                call.criteria = Some("jammer_active".into());
                vec![call]
            })
            .unwrap_or_default(),
        "recon_flank_route" => {
            let mut calls = Vec::new();
            if let (Some(own), Some(target)) = (ctx.own_pose, ctx.target_pose) {
                let waypoints = flank_route(&FlankRequest {
                    own,
                    target_lat: target.lat,
                    target_lon: target.lon,
                    target_alt_m: target.alt,
                    target_heading_deg: target.heading_deg,
                    standoff_m: ctx.standoff_m,
                    side: ctx.flank_side,
                    speed_ms: ctx.speed_ms,
                });
                calls.push(mk(
                    "route",
                    PlatformCommandSpec::FollowRoute { waypoints },
                    false,
                ));
            }
            calls.push(mk(
                "sensor",
                PlatformCommandSpec::SensorOn {
                    sensor_id: ctx.sensor_id.clone(),
                },
                false,
            ));
            calls
        }
        "deploy_recon_uav" | "release_recon_slot" => ctx
            .target_track_id
            .and_then(|track_id| {
                ctx.scout_uav_weapon_id.as_ref().map(|weapon_id| {
                    // Deploying a scout UAV lowers to FireAtTarget on the wire but
                    // is an ISR action, not a kinetic release — mark it non-lethal
                    // so it bypasses the lethal release-geometry / standoff gate.
                    let mut call = mk(
                        "employ",
                        PlatformCommandSpec::Fire {
                            weapon_id: weapon_id.clone(),
                            track_id: track_id.to_string(),
                            salvo_size: None,
                        },
                        false,
                    );
                    if play_id == "ReactiveDefense" {
                        call.preconditions = vec!["event:missile_inbound".into()];
                        call.criteria = Some("recon_uav_launched".into());
                    }
                    vec![call]
                })
            })
            .unwrap_or_default(),
        "surveil_target_area" => {
            let mut calls = vec![mk(
                "sensor_track",
                PlatformCommandSpec::SensorSetMode {
                    sensor_id: ctx.sensor_id.clone(),
                    mode: "track".into(),
                },
                false,
            )];
            if let Some(track_id) = ctx.target_track_id {
                calls.push(mk(
                    "track",
                    PlatformCommandSpec::Designate {
                        track_id: track_id.to_string(),
                    },
                    false,
                ));
            }
            calls
        }
        "track_target_area" => ctx
            .target_track_id
            .map(|track_id| {
                vec![mk(
                    "track",
                    PlatformCommandSpec::Designate {
                        track_id: track_id.to_string(),
                    },
                    false,
                )]
            })
            .unwrap_or_default(),
        "sensor_on" => vec![mk(
            "on",
            PlatformCommandSpec::SensorOn {
                sensor_id: ctx.sensor_id.clone(),
            },
            false,
        )],
        "patrol_leg" => {
            let mut calls = Vec::new();
            if let Some(own) = ctx.own_pose {
                if let Some(radius_m) = ctx.patrol_radius_m {
                    calls.push(mk(
                        "circle",
                        PlatformCommandSpec::FollowRoute {
                            waypoints: circular_patrol_route(own, radius_m, ctx.speed_ms),
                        },
                        false,
                    ));
                } else {
                    let (lat, lon) = crate::flank_geometry::destination(
                        own.lat_deg,
                        own.lon_deg,
                        own.heading_deg,
                        5000.0,
                    );
                    calls.push(mk(
                        "leg",
                        PlatformCommandSpec::Goto {
                            lat,
                            lon,
                            alt: Some(own.alt_m),
                            speed_ms: ctx.speed_ms,
                        },
                        false,
                    ));
                }
            }
            calls.push(mk(
                "sensor",
                PlatformCommandSpec::SensorOn {
                    sensor_id: ctx.sensor_id.clone(),
                },
                false,
            ));
            calls
        }
        "track_sensor" => vec![mk(
            "track",
            PlatformCommandSpec::SensorSetMode {
                sensor_id: ctx.sensor_id.clone(),
                mode: "track".into(),
            },
            false,
        )],
        // One Designate per (engaged) target — designate all before firing.
        "designate" => ctx
            .designate_targets
            .iter()
            .enumerate()
            .map(|(i, track_id)| {
                mk(
                    &format!("designate:{i}"),
                    PlatformCommandSpec::Designate {
                        track_id: track_id.clone(),
                    },
                    false,
                )
            })
            .collect(),
        // One coordinated strike per engaged target.
        "coordinated_strike" => {
            if ctx.has_weapon {
                ctx.fire_targets
                    .iter()
                    .enumerate()
                    .map(|(i, track_id)| {
                        mk(
                            &format!("strike:{i}"),
                            PlatformCommandSpec::CoordinatedStrike {
                                strike_platform_ids: vec![platform_id.to_string()],
                                target_id: track_id.clone(),
                                time_on_target_us: ((ctx.timestamp + 60.0) * 1_000_000.0) as u64,
                            },
                            true,
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        // One Fire per engaged target (inventory-capped upstream).
        "fire" => {
            if ctx.has_weapon {
                ctx.fire_targets
                    .iter()
                    .enumerate()
                    .map(|(i, track_id)| {
                        mk(
                            &format!("fire:{i}"),
                            PlatformCommandSpec::Fire {
                                weapon_id: ctx.weapon_id.clone(),
                                track_id: track_id.clone(),
                                salvo_size: ctx.salvo_size,
                            },
                            true,
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        "bda" => vec![mk(
            "bda",
            PlatformCommandSpec::SensorOn {
                sensor_id: ctx.sensor_id.clone(),
            },
            false,
        )],
        "rtb" => {
            let (lat, lon, alt) = ctx
                .home
                .or_else(|| ctx.own_pose.map(|p| (p.lat_deg, p.lon_deg, Some(p.alt_m))))
                .unwrap_or((0.0, 0.0, None));
            vec![mk(
                "rtb",
                PlatformCommandSpec::Goto {
                    lat,
                    lon,
                    alt,
                    speed_ms: ctx.speed_ms,
                },
                false,
            )]
        }
        _ => Vec::new(),
    }
}

/// Lower parsed motion slots into concrete platform commands (heading, speed,
/// flank route). Prepended before play functions so maneuver precedes sensor/fire.
fn inject_maneuver_functions(
    intent: &StructuredIntent,
    ctx: &FnCtx<'_>,
    functions: &mut Vec<FunctionCall>,
) {
    if !intent.maneuver.is_active() {
        return;
    }
    let m = &intent.maneuver;
    let mut injected = Vec::new();
    let mk = |suffix: &str, command: PlatformCommandSpec| {
        let id = format!("maneuver:{suffix}");
        FunctionCall {
            id: id.clone(),
            task_id: id,
            parent_play: "Maneuver".into(),
            platform_id: ctx.own_platform_id.to_string(),
            command,
            preconditions: Vec::new(),
            criteria: None,
            phase: 0,
            ordering: 0,
            service: None,
            safety_guard: SafetyGuard::default(),
        }
    };

    let has_route = functions
        .iter()
        .any(|f| matches!(f.command, PlatformCommandSpec::FollowRoute { .. }));
    let wants_flank = m.flank_approach || intent.flank_side.is_some();

    if wants_flank && !has_route {
        if let (Some(own), Some(target)) = (ctx.own_pose, ctx.target_pose) {
            let waypoints = flank_route(&FlankRequest {
                own,
                target_lat: target.lat,
                target_lon: target.lon,
                target_alt_m: target.alt,
                target_heading_deg: target.heading_deg,
                standoff_m: ctx.standoff_m,
                side: intent.flank_side.or(m.turn),
                speed_ms: m.speed_ms.or(ctx.speed_ms),
            });
            injected.push(mk(
                "flank_route",
                PlatformCommandSpec::FollowRoute { waypoints },
            ));
        }
    }

    let has_speed_cmd = functions
        .iter()
        .any(|f| matches!(f.command, PlatformCommandSpec::SetSpeed { .. }));
    if let Some(speed_ms) = m.speed_ms {
        if !has_speed_cmd {
            injected.push(mk("speed", PlatformCommandSpec::SetSpeed { speed_ms }));
        }
    }

    let has_heading_cmd = functions
        .iter()
        .any(|f| matches!(f.command, PlatformCommandSpec::SetHeading { .. }));

    if !has_heading_cmd {
        if let Some(heading_deg) = m.heading_deg {
            injected.push(mk(
                "heading_abs",
                PlatformCommandSpec::SetHeading {
                    heading_deg,
                    speed_ms: m.speed_ms.or(ctx.speed_ms),
                    turn_direction: m.turn.map(flank_side_to_turn),
                },
            ));
        } else if let Some(delta) = m.heading_delta_deg {
            if let Some(own) = ctx.own_pose {
                injected.push(mk(
                    "heading_rel",
                    PlatformCommandSpec::SetHeading {
                        heading_deg: normalize_heading_deg(own.heading_deg + delta),
                        speed_ms: m.speed_ms.or(ctx.speed_ms),
                        turn_direction: m.turn.map(flank_side_to_turn),
                    },
                ));
            }
        }
    }

    for call in injected.into_iter().rev() {
        functions.insert(0, call);
    }
}

fn flank_side_to_turn(side: FlankSide) -> TurnDirection {
    match side {
        FlankSide::Left => TurnDirection::Left,
        FlankSide::Right => TurnDirection::Right,
    }
}

fn normalize_heading_deg(deg: f64) -> f64 {
    let mut h = deg % 360.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

// ─────────────────────────────────────────────
// Constraints / objectives / intervention / trace
// ─────────────────────────────────────────────

fn build_constraints(
    roe: Option<WeaponReleaseLevel>,
    standoff_m: f64,
    pid_required: bool,
    has_lethal: bool,
) -> Vec<Constraint> {
    let mut constraints = vec![Constraint::standoff(standoff_m, has_lethal)];
    if let Some(level) = roe {
        constraints.push(Constraint::roe(level));
    }
    if has_lethal && pid_required {
        constraints.push(Constraint::pid_required());
    }
    constraints
}

fn build_intervention_points(has_lethal: bool) -> Vec<InterventionPoint> {
    let mut points = vec![InterventionPoint {
        on: "plan_deviation".into(),
        level: InterventionLevel::Monitor,
        action: InterventionAction::Notify,
    }];
    if has_lethal {
        points.push(InterventionPoint::require_approval_before_fire());
    }
    points
}

fn build_objectives(
    kind: MissionKind,
    target_track_id: Option<&str>,
    own_platform_id: &str,
) -> Vec<DslObjective> {
    let (description, feedback_var, priority) = match kind {
        MissionKind::Engage | MissionKind::CoordinatedStrike | MissionKind::ReconFlankStrike => {
            let target = target_track_id.unwrap_or("designated target");
            (
                format!("neutralize {target}"),
                target_track_id.map(|t| format!("track:{t}:engaged")),
                100,
            )
        }
        MissionKind::Recon => {
            let target = target_track_id.unwrap_or("area of interest");
            (
                format!("maintain ISR coverage of {target}"),
                Some(
                    target_track_id
                        .map(|t| format!("track:{t}:tracked"))
                        .unwrap_or_else(|| "isr_coverage".into()),
                ),
                60,
            )
        }
        MissionKind::Track => {
            let target = target_track_id.unwrap_or("contact");
            (
                format!("maintain track on {target}"),
                target_track_id.map(|t| format!("track:{t}:tracked")),
                50,
            )
        }
        MissionKind::Patrol => (
            "patrol assigned area".into(),
            Some("isr_coverage".into()),
            20,
        ),
        MissionKind::Rtb => (
            "return to base".into(),
            Some(format!("platform:{own_platform_id}:rtb")),
            30,
        ),
        MissionKind::PointDefense => {
            let target = target_track_id.unwrap_or("inbound threat");
            (
                format!("defeat {target}"),
                Some(
                    target_track_id
                        .map(|t| format!("track:{t}:engaged"))
                        .unwrap_or_else(|| format!("platform:{own_platform_id}:self_protected")),
                ),
                95,
            )
        }
        MissionKind::TargetingHandoff => {
            let target = target_track_id.unwrap_or("contact");
            (
                format!("provide targeting-quality track on {target}"),
                target_track_id.map(|t| format!("track:{t}:handed_off")),
                80,
            )
        }
        MissionKind::Picket => (
            "hold picket station and provide early warning".into(),
            Some("isr_coverage".into()),
            55,
        ),
        MissionKind::Escort => (
            "screen and protect the escorted unit".into(),
            Some(format!("platform:{own_platform_id}:escort_station")),
            65,
        ),
        MissionKind::MaritimeInterdiction => {
            let target = target_track_id.unwrap_or("contact of interest");
            (
                format!("interdict and deny passage to {target}"),
                target_track_id.map(|t| format!("track:{t}:interdicted")),
                70,
            )
        }
        MissionKind::Deception => (
            "execute deception to shape adversary behavior".into(),
            Some("deception_effect".into()),
            40,
        ),
        MissionKind::SensorControl => (
            "operator sensor control".into(),
            Some(format!("platform:{own_platform_id}:sensor_control")),
            45,
        ),
        MissionKind::ReactiveDefense => (
            "survive inbound threat with coordinated evade, soft-kill and ISR support".into(),
            Some(format!("platform:{own_platform_id}:survived")),
            95,
        ),
        MissionKind::Unknown => (
            "clarify operator intent".into(),
            Some("intent_classified".into()),
            10,
        ),
    };
    vec![DslObjective {
        id: format!("obj:{}", kind.label().to_lowercase()),
        description,
        feedback_var,
        priority,
    }]
}

fn build_explanation_trace(
    kind: MissionKind,
    plays: &[PlayInstance],
    functions: &[FunctionCall],
) -> String {
    let play_ids: Vec<String> = {
        let mut seen = Vec::new();
        for p in plays {
            if !seen.contains(&p.play_id) {
                seen.push(p.play_id.clone());
            }
        }
        seen
    };
    let fn_labels: Vec<&str> = functions.iter().map(|f| f.command.label()).collect();
    format!(
        "M({}) → Plays[{}] → Funcs[{}]",
        kind.label(),
        play_ids.join(", "),
        fn_labels.join(", ")
    )
}

/// Resolve a target identifier to a REAL firing track id present in the
/// snapshot. AFSIM only accepts weapon/sensor track ids that name a track a
/// platform actually holds (`<name>:<number>`); anything else logs
/// `TrackID is error:...` and can crash Warlock.
///
/// Order:
///   1. exact match on a track's `track_id` → already valid, use as-is;
///   2. reverse lookup by truth name (`target_name`, ArkSIM `targetName`) →
///      maps "blue_patrol_3" back to the real "self:3";
///   3. hostile/neutral platform id or name → prefer a sensor track that
///      designates that platform, else the platform id (planning layer uses
///      the same convention for opportunity `track_id`);
///   4. otherwise `None` — no real target exists for the target, so the caller
///      drops the target rather than fabricating an invalid one.
fn resolve_track_id(snapshot: &WorldSnapshot, raw_id: &str) -> Option<String> {
    if raw_id.is_empty() {
        return None;
    }
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if track.track_id == raw_id {
                return Some(track.track_id.clone());
            }
        }
    }
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if !track.target_name.is_empty() && track.target_name == raw_id {
                return Some(track.track_id.clone());
            }
        }
    }
    let platform = snapshot
        .find_platform(raw_id)
        .or_else(|| snapshot.platforms.iter().find(|p| p.name == raw_id))?;
    let prefix = format!("{}:", platform.id);
    for observer in &snapshot.platforms {
        for track in &observer.tracks {
            if track.target_name == platform.id
                || track.target_name == platform.name
                || track.track_id.starts_with(&prefix)
            {
                return Some(track.track_id.clone());
            }
        }
    }
    Some(platform.id.clone())
}

fn resolve_target_pose(snapshot: &WorldSnapshot, target_id: &str) -> Option<TargetPose> {
    // Prefer a track with matching id anywhere in the snapshot.
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if track.track_id == target_id {
                if let Some((lat, lon, alt)) = track.position_lla {
                    return Some(TargetPose {
                        lat,
                        lon,
                        alt: Some(alt),
                        heading_deg: track.heading_deg,
                    });
                }
            }
        }
    }
    // Fall back to a platform whose id equals the target id (hostile platform).
    snapshot.find_platform(target_id).map(|p| TargetPose {
        lat: p.pose.lat_deg,
        lon: p.pose.lon_deg,
        alt: Some(p.pose.alt_m),
        heading_deg: Some(p.pose.heading_deg),
    })
}

fn mission_id(intent_text: &str, timestamp: f64) -> String {
    let mut hasher = DefaultHasher::new();
    intent_text.hash(&mut hasher);
    (timestamp as i64).hash(&mut hasher);
    format!("mission:{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent_extractor::IntentExtractor;
    use crate::platform_allocator::SelfPlatformAllocator;
    use async_trait::async_trait;
    use openfang_types::config::{ControlledSide, ThreatSide};
    use openfang_types::platform::{
        Affiliation, Domain, FuelStatus, JammerState, PlatformState, Pose, SensorState, SensorType,
        Track, Velocity, WeaponState,
    };
    use openfang_types::semantic_frame::{Effect, ObjectKind};

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
        own.onboard_sensors = vec![SensorState {
            sensor_id: "eo1".into(),
            sensor_type: SensorType::EOIR,
            mode: "search".into(),
            frequency_hz: None,
            bandwidth_hz: None,
            azimuth_fov_deg: None,
            elevation_fov_deg: None,
            range_max_m: Some(15_000.0),
            damage: 0.0,
            host_platform_id: "red-uav-1".into(),
        }];
        own.tracks = vec![Track {
            track_id: "blue_command_post:1".into(),
            target_name: "blue_command_post".into(),
            classification: "command_post".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((30.05, 120.05, 0.0)),
            heading_deg: Some(0.0),
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
            timestamp: 100.0,
            platforms: vec![own, blue],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn compile_text(text: &str) -> MissionDsl {
        let snap = snapshot();
        let policy = policy();
        let intent = IntentExtractor::new().extract(text, &snap, &policy);
        let registry = PlayRegistry::bundled();
        let compiler = MissionCompiler::new(SelfPlatformAllocator::new());
        compiler.compile(
            &intent,
            &snap,
            &registry,
            &policy,
            &CompileParams::default(),
        )
    }

    fn symbolic_task(
        task_id: &str,
        platform: &str,
        action: &str,
        preconditions: &[&str],
        parameters: serde_json::Map<String, serde_json::Value>,
        ordering: u32,
    ) -> SymbolicTask {
        SymbolicTask {
            task_id: task_id.into(),
            platform: Some(platform.into()),
            action: action.into(),
            target: None,
            criteria: Some("route_completed".into()),
            preconditions: preconditions
                .iter()
                .map(|value| value.to_string())
                .collect(),
            parameters,
            phase: 0,
            ordering,
        }
    }

    struct MockIntentDriver {
        intent: StructuredIntent,
    }

    #[async_trait]
    impl IntentExtractDriver for MockIntentDriver {
        async fn extract(
            &self,
            _ctx: crate::intent_extractor::ExtractContext,
        ) -> Option<StructuredIntent> {
            Some(self.intent.clone())
        }
    }

    fn llm_intent(kind: MissionKind, tasks: Vec<SymbolicTask>) -> StructuredIntent {
        let mut intent = StructuredIntent::unknown("mock llm objective");
        intent.kind = kind;
        intent.confidence = 0.95;
        intent.semantic_source = crate::intent_extractor::IntentSemanticSource::Llm;
        intent.task_plan = tasks;
        intent
    }

    #[test]
    fn symbolic_plan_binds_heterogeneous_platforms_and_grounded_speed() {
        let mut snap = snapshot();
        let mut usv = PlatformState::minimal("red-usv-1");
        usv.name = "harbor-usv".into();
        usv.affiliation = Affiliation::Red;
        usv.platform_type = "usv".into();
        usv.domain = Domain::Surface;
        let mut uav = PlatformState::minimal("red-uav-2");
        uav.name = "overwatch-uav".into();
        uav.affiliation = Affiliation::Red;
        uav.platform_type = "uav".into();
        uav.domain = Domain::Air;
        uav.onboard_sensors = vec![SensorState {
            sensor_id: "uav-eo".into(),
            sensor_type: SensorType::EOIR,
            mode: "search".into(),
            frequency_hz: None,
            bandwidth_hz: None,
            azimuth_fov_deg: None,
            elevation_fov_deg: None,
            range_max_m: Some(8_000.0),
            damage: 0.0,
            host_platform_id: "red-uav-2".into(),
        }];
        snap.platforms.extend([usv, uav]);

        let mut max_speed = serde_json::Map::new();
        max_speed.insert("speed".into(), serde_json::Value::String("max".into()));
        let mut intent =
            StructuredIntent::unknown("检查船舶装卸区域，确保起重机附近没有人员或车辆");
        intent.kind = MissionKind::Recon;
        intent.task_plan = vec![
            SymbolicTask {
                task_id: "T1".into(),
                platform: Some("USV".into()),
                action: "Goto".into(),
                target: Some("ship_loading_area".into()),
                criteria: Some("position_reached".into()),
                preconditions: Vec::new(),
                parameters: max_speed,
                phase: 0,
                ordering: 0,
            },
            SymbolicTask {
                task_id: "T2".into(),
                platform: Some("UAV".into()),
                action: "SensorOn".into(),
                target: None,
                criteria: Some("sensor_active".into()),
                preconditions: vec!["T1_complete".into()],
                parameters: serde_json::Map::from_iter([(
                    "sensor".into(),
                    serde_json::Value::String("eoir".into()),
                )]),
                phase: 1,
                ordering: 1,
            },
            SymbolicTask {
                task_id: "T3".into(),
                platform: Some("UAV".into()),
                action: "Goto".into(),
                target: Some("crane_overview_point".into()),
                criteria: Some("position_reached".into()),
                preconditions: vec!["T2_complete".into()],
                parameters: serde_json::Map::new(),
                phase: 2,
                ordering: 2,
            },
            SymbolicTask {
                task_id: "T4".into(),
                platform: Some("UAV".into()),
                action: "SensorSetMode".into(),
                target: Some("crane".into()),
                criteria: Some("human/vehicle near crane".into()),
                preconditions: vec!["T3_complete".into()],
                parameters: serde_json::Map::from_iter([
                    ("sensor".into(), serde_json::Value::String("eoir".into())),
                    ("mode".into(), serde_json::Value::String("track".into())),
                ]),
                phase: 3,
                ordering: 3,
            },
        ];
        let policy = PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            controlled_platforms: vec!["red-usv-1".into(), "red-uav-2".into()],
            own_platform_id: "red-usv-1".into(),
            ..policy()
        };
        let params = CompileParams {
            max_speed_ms: 12.5,
            ..CompileParams::default()
        };

        let mission = MissionCompiler::new(SelfPlatformAllocator::new()).compile(
            &intent,
            &snap,
            &PlayRegistry::bundled(),
            &policy,
            &params,
        );

        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
        assert_eq!(mission.functions.len(), 4);
        assert_eq!(mission.functions[0].task_id, "T1");
        assert_eq!(mission.functions[0].platform_id, "red-usv-1");
        assert!(matches!(
            mission.functions[0].command,
            PlatformCommandSpec::Goto {
                speed_ms: Some(12.5),
                ..
            }
        ));
        assert_eq!(mission.functions[1].platform_id, "red-uav-2");
        assert_eq!(mission.functions[1].preconditions, vec!["T1_complete"]);
        assert_eq!(mission.functions[2].platform_id, "red-uav-2");
        assert_eq!(mission.functions[2].preconditions, vec!["T2_complete"]);
        assert_eq!(mission.functions[3].platform_id, "red-uav-2");
        assert_eq!(mission.functions[3].preconditions, vec!["T3_complete"]);
        assert!(matches!(
            &mission.functions[3].command,
            PlatformCommandSpec::SensorSetMode { sensor_id, .. } if sensor_id == "uav-eo"
        ));
    }

    #[tokio::test]
    async fn mock_llm_follow_route_circle_radius_to_mms_route() {
        let mut params = serde_json::Map::new();
        params.insert(
            "route_shape".into(),
            serde_json::Value::String("circle".into()),
        );
        params.insert(
            "center".into(),
            serde_json::Value::String("current_position".into()),
        );
        params.insert("radius_m".into(), serde_json::json!(100_000.0));
        params.insert("speed".into(), serde_json::Value::String("max".into()));
        let mut task = symbolic_task("T1", "red-uav-1", "FollowRoute", &[], params, 0);
        task.criteria = Some("route_started".into());
        let intent = llm_intent(MissionKind::Patrol, vec![task]);
        let driver = MockIntentDriver { intent };
        let snap = snapshot();
        let policy = policy();
        let output = compile_objective_with_semantics(
            "巡逻周边100km半径的圆形海域",
            &snap,
            &policy,
            &PlayRegistry::bundled(),
            &CompileParams::default(),
            Some(&driver),
            0.5,
        )
        .await;
        assert!(
            output.validation_issues.is_empty(),
            "issues: {:?}",
            output.validation_issues
        );
        let mission = output.mission;
        assert_eq!(mission.kind, MissionKind::Patrol);
        let route = mission
            .functions
            .iter()
            .find(|function| matches!(function.command, PlatformCommandSpec::FollowRoute { .. }))
            .expect("expected circular patrol route");

        assert_eq!(
            route.service(),
            openfang_types::mission_dsl::MissionService::Mms
        );
        if let PlatformCommandSpec::FollowRoute { waypoints } = &route.command {
            assert_eq!(waypoints.len(), 9);
            let first = waypoints.first().expect("first waypoint");
            let last = waypoints.last().expect("closing waypoint");
            assert!((first.lat - last.lat).abs() < 1e-9);
            assert!((first.lon - last.lon).abs() < 1e-9);
            let own = snapshot()
                .find_platform("red-uav-1")
                .expect("own platform")
                .pose;
            let first_pose = Pose {
                lat_deg: first.lat,
                lon_deg: first.lon,
                alt_m: first.alt.unwrap_or(own.alt_m),
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            };
            assert!((own.distance_m(&first_pose) - 100_000.0).abs() < 100.0);
        }
        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    }

    #[test]
    fn symbolic_send_message_lowers_to_cms_arksim_command() {
        let params = serde_json::Map::from_iter([
            (
                "to_platform_id".into(),
                serde_json::Value::String("blue_command_post".into()),
            ),
            (
                "message".into(),
                serde_json::Value::String("Patrol started".into()),
            ),
        ]);
        let mut task = symbolic_task("T1", "red-uav-1", "SendMessage", &[], params, 0);
        task.criteria = Some("message_sent".into());
        let intent = llm_intent(MissionKind::Patrol, vec![task]);
        let snap = snapshot();
        let policy = policy();
        let mission = MissionCompiler::new(SelfPlatformAllocator::new()).compile(
            &intent,
            &snap,
            &PlayRegistry::bundled(),
            &policy,
            &CompileParams::default(),
        );

        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
        let message = mission
            .functions
            .iter()
            .find(|function| matches!(function.command, PlatformCommandSpec::SendMessage { .. }))
            .expect("expected SendMessage function");
        assert_eq!(
            message.service(),
            openfang_types::mission_dsl::MissionService::Cms
        );
        match message.command.to_platform_command(&message.platform_id) {
            openfang_types::platform::PlatformCommand::SendMessage {
                from_platform_id,
                to_platform_id,
                message,
            } => {
                assert_eq!(from_platform_id, "red-uav-1");
                assert_eq!(to_platform_id, "blue_command_post");
                assert_eq!(message, "Patrol started");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_llm_driver_does_not_keyword_compile_dsl() {
        let snap = snapshot();
        let policy = policy();
        let output = compile_objective_with_semantics(
            "巡逻周边100km半径的圆形海域",
            &snap,
            &policy,
            &PlayRegistry::bundled(),
            &CompileParams::default(),
            None,
            0.5,
        )
        .await;

        assert_eq!(output.structured_intent.kind, MissionKind::Unknown);
        assert!(output.mission.functions.is_empty());
        assert!(output
            .validation_issues
            .iter()
            .any(|issue| issue.message.contains("no executable mission_plan.tasks")));
    }

    #[test]
    fn symbolic_validation_rejects_illegal_action_and_dangling_dependency() {
        let mut intent = llm_intent(
            MissionKind::Recon,
            vec![SymbolicTask {
                task_id: "T1".into(),
                platform: Some("red-uav-1".into()),
                action: "Navigate".into(),
                target: None,
                criteria: Some("position_reached".into()),
                preconditions: vec!["T0_complete".into()],
                parameters: serde_json::Map::new(),
                phase: 0,
                ordering: 1,
            }],
        );
        intent.raw_text = "invalid llm task".into();
        let issues = validate_symbolic_task_plan(&intent, &snapshot(), &policy());

        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("unsupported action")));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("unknown precondition")));
    }

    #[test]
    fn symbolic_validation_rejects_cycle_and_motion_frontier_conflict() {
        let mut route_params = serde_json::Map::new();
        route_params.insert(
            "route_shape".into(),
            serde_json::Value::String("circle".into()),
        );
        route_params.insert("radius_m".into(), serde_json::json!(1000.0));
        let mut goto_params = serde_json::Map::new();
        goto_params.insert("lat".into(), serde_json::json!(30.01));
        goto_params.insert("lon".into(), serde_json::json!(120.01));

        let intent = llm_intent(
            MissionKind::Patrol,
            vec![
                symbolic_task(
                    "T1",
                    "red-uav-1",
                    "FollowRoute",
                    &["T2_complete"],
                    route_params,
                    1,
                ),
                symbolic_task(
                    "T2",
                    "red-uav-1",
                    "Goto",
                    &["T1_complete"],
                    goto_params.clone(),
                    1,
                ),
                symbolic_task(
                    "T3",
                    "red-uav-1",
                    "Goto",
                    &["event:missile_inbound"],
                    goto_params.clone(),
                    0,
                ),
                symbolic_task(
                    "T4",
                    "red-uav-1",
                    "SetSpeed",
                    &["event:missile_inbound"],
                    serde_json::Map::from_iter([("speed".into(), serde_json::json!(10.0))]),
                    0,
                ),
            ],
        );
        let issues = validate_symbolic_task_plan(&intent, &snapshot(), &policy());

        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("contains a cycle")));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("motion lane")));
    }

    #[test]
    fn compiles_recon_flank_strike_with_route_and_lethal_gate() {
        let mission =
            compile_text("绕后使用侦察无人机察打一体打击蓝方指挥所，注意保持安全距离3公里");
        assert_eq!(mission.kind, MissionKind::ReconFlankStrike);
        // Standoff constraint reflects the parsed 3 km.
        assert_eq!(mission.standoff_m(), Some(3000.0));
        // A flank route was generated.
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::FollowRoute { .. })));
        // Lethal coordinated strike present → approval intervention point + valid.
        assert!(mission.has_lethal_function());
        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    }

    #[test]
    fn compiles_engage_and_is_valid() {
        let mission = compile_text("打击蓝方指挥所");
        assert_eq!(mission.kind, MissionKind::Engage);
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::Fire { .. })));
        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    }

    #[test]
    fn explicit_radar_on_selects_radar_sensor_not_first_eoir() {
        let mut snap = snapshot();
        let own = snap
            .platforms
            .iter_mut()
            .find(|p| p.id == "red-uav-1")
            .expect("own platform");
        own.onboard_sensors.push(SensorState {
            sensor_id: "surf_radar".into(),
            sensor_type: SensorType::Radar,
            mode: "standby".into(),
            frequency_hz: None,
            bandwidth_hz: None,
            azimuth_fov_deg: None,
            elevation_fov_deg: None,
            range_max_m: Some(30_000.0),
            damage: 0.0,
            host_platform_id: "red-uav-1".into(),
        });

        let policy = policy();
        let intent = IntentExtractor::new().extract("打开雷达", &snap, &policy);
        let mission = MissionCompiler::new(SelfPlatformAllocator::new()).compile(
            &intent,
            &snap,
            &PlayRegistry::bundled(),
            &policy,
            &CompileParams::default(),
        );

        assert_eq!(mission.kind, MissionKind::SensorControl);
        assert!(mission.functions.iter().any(|f| matches!(
            &f.command,
            PlatformCommandSpec::SensorOn { sensor_id } if sensor_id == "surf_radar"
        )));
        assert!(!mission.functions.iter().any(|f| matches!(
            &f.command,
            PlatformCommandSpec::SensorOn { sensor_id } if sensor_id == "eo1"
        )));
    }

    fn patrol_track(track_id: &str, name: &str) -> Track {
        Track {
            track_id: track_id.into(),
            target_name: name.into(),
            classification: "surface_combatant".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((30.05, 120.05, 0.0)),
            heading_deg: Some(0.0),
            speed_ms: None,
            range_m: Some(5_000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        }
    }

    fn engage_intent(targets: &[&str]) -> StructuredIntent {
        let mut intent = StructuredIntent::unknown("打击蓝方水面舰艇");
        intent.kind = MissionKind::Engage;
        intent.confidence = 0.9;
        intent.target_track_ids = targets.iter().map(|s| s.to_string()).collect();
        intent
    }

    #[test]
    fn multi_target_engage_fires_at_every_resolved_target() {
        // Regression for "语义识别多个目标但只打第一个": a plural strike intent
        // must engage every grounded target, not just the first.
        let mut snap = snapshot();
        let own = snap
            .platforms
            .iter_mut()
            .find(|p| p.id == "red-uav-1")
            .unwrap();
        own.tracks = vec![
            patrol_track("self:1", "blue_patrol_1"),
            patrol_track("self:2", "blue_patrol_2"),
            patrol_track("self:3", "blue_patrol_3"),
        ];
        own.onboard_weapons[0].quantity_remaining = 3.0;

        let intent = engage_intent(&["blue_patrol_1", "blue_patrol_2", "blue_patrol_3"]);
        let mission = MissionCompiler::new(SelfPlatformAllocator::new()).compile(
            &intent,
            &snap,
            &PlayRegistry::bundled(),
            &policy(),
            &CompileParams::default(),
        );

        let fires: Vec<&str> = mission
            .functions
            .iter()
            .filter_map(|f| match &f.command {
                PlatformCommandSpec::Fire { track_id, .. } => Some(track_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fires.len(), 3, "every resolved target engaged: {fires:?}");
        assert!(fires.contains(&"self:1"));
        assert!(fires.contains(&"self:2"));
        assert!(fires.contains(&"self:3"));
        assert!(!mission.explanation_trace.contains("因弹药不足未交战"));
    }

    #[test]
    fn multi_target_engage_caps_fires_at_weapon_inventory() {
        // Inventory ceiling: two rounds, three targets ⇒ two fires + the third
        // target explicitly annotated as un-engaged (never silently dropped).
        let mut snap = snapshot();
        let own = snap
            .platforms
            .iter_mut()
            .find(|p| p.id == "red-uav-1")
            .unwrap();
        own.tracks = vec![
            patrol_track("self:1", "blue_patrol_1"),
            patrol_track("self:2", "blue_patrol_2"),
            patrol_track("self:3", "blue_patrol_3"),
        ];
        own.onboard_weapons[0].quantity_remaining = 2.0;

        let intent = engage_intent(&["blue_patrol_1", "blue_patrol_2", "blue_patrol_3"]);
        let mission = MissionCompiler::new(SelfPlatformAllocator::new()).compile(
            &intent,
            &snap,
            &PlayRegistry::bundled(),
            &policy(),
            &CompileParams::default(),
        );

        let fires = mission
            .functions
            .iter()
            .filter(|f| matches!(f.command, PlatformCommandSpec::Fire { .. }))
            .count();
        assert_eq!(fires, 2, "fires capped at inventory");
        assert!(
            mission.explanation_trace.contains("因弹药不足未交战"),
            "trace: {}",
            mission.explanation_trace
        );
        assert!(
            mission.explanation_trace.contains("self:3"),
            "third target annotated: {}",
            mission.explanation_trace
        );
    }

    #[test]
    fn recon_uav_intent_employs_scout_uav_slot() {
        let mut snap = snapshot();
        let own = snap
            .platforms
            .iter_mut()
            .find(|p| p.id == "red-uav-1")
            .expect("own platform");
        own.onboard_weapons.push(WeaponState {
            weapon_id: "scout_uav_slot".into(),
            weapon_type: "SCOUT_UAV_SLOT".into(),
            quantity_remaining: 2.0,
            max_range_m: Some(30_000.0),
            min_range_m: Some(0.0),
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        });

        let policy = policy();
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("发射侦察无人机侦察蓝方指挥所并持续监视", &snap, &policy);
        let frame = crate::intent_extractor::to_commander_frame(&intent);
        assert!(matches!(
            frame.effect,
            Effect::Surveil | Effect::Reconnoiter
        ));
        assert!(frame
            .objects
            .iter()
            .any(|object| object.kind == ObjectKind::Asset
                && object.label.as_deref() == Some("scout_uav_slot")));

        let registry = PlayRegistry::bundled();
        let compiler = MissionCompiler::new(SelfPlatformAllocator::new());
        let mission =
            compiler.compile_frame(&frame, &snap, &registry, &policy, &CompileParams::default());

        assert_eq!(mission.kind, MissionKind::Recon);
        assert!(mission.functions.iter().any(|f| matches!(
            &f.command,
            PlatformCommandSpec::Fire {
                weapon_id,
                track_id,
                salvo_size: None
            } if weapon_id == "scout_uav_slot" && track_id == "blue_command_post:1"
        )));
    }

    #[test]
    fn recon_uav_intent_employs_j7_uav_weapon_slot() {
        let mut snap = snapshot();
        let own = snap
            .platforms
            .iter_mut()
            .find(|p| p.id == "red-uav-1")
            .expect("own platform");
        own.onboard_weapons.retain(|weapon| {
            !format!("{} {}", weapon.weapon_id, weapon.weapon_type)
                .to_ascii_lowercase()
                .contains("scout_uav")
        });
        own.onboard_weapons.push(WeaponState {
            weapon_id: "J7_UAV_WEAPON".into(),
            weapon_type: "J7_UAV_WEAPON".into(),
            quantity_remaining: 2.0,
            max_range_m: Some(30_000.0),
            min_range_m: Some(0.0),
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        });

        let policy = policy();
        let extractor = IntentExtractor::new();
        let intent = extractor.extract("发射侦察无人机侦察蓝方指挥所并持续监视", &snap, &policy);
        let frame = crate::intent_extractor::to_commander_frame(&intent);
        let registry = PlayRegistry::bundled();
        let compiler = MissionCompiler::new(SelfPlatformAllocator::new());
        let mission =
            compiler.compile_frame(&frame, &snap, &registry, &policy, &CompileParams::default());

        assert!(mission.functions.iter().any(|f| matches!(
            &f.command,
            PlatformCommandSpec::Fire {
                weapon_id,
                track_id,
                salvo_size: None
            } if weapon_id == "J7_UAV_WEAPON" && track_id == "blue_command_post:1"
        )));
    }

    #[test]
    fn reactive_defense_compiles_parallel_mms_ewms_wms_tasks() {
        let mut snap = snapshot();
        let own = snap
            .platforms
            .iter_mut()
            .find(|p| p.id == "red-uav-1")
            .expect("own platform");
        own.onboard_weapons.push(WeaponState {
            weapon_id: "chaff_1".into(),
            weapon_type: "CHAFF_DECOY".into(),
            quantity_remaining: 4.0,
            max_range_m: None,
            min_range_m: None,
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        });
        own.onboard_weapons.push(WeaponState {
            weapon_id: "scout_uav_slot".into(),
            weapon_type: "SCOUT_UAV_SLOT".into(),
            quantity_remaining: 1.0,
            max_range_m: Some(30_000.0),
            min_range_m: Some(0.0),
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        });
        own.onboard_jammers.push(JammerState {
            jammer_id: "jam_1".into(),
            host_id: "red-uav-1".into(),
            is_active: false,
            beams: vec![],
        });

        let policy = policy();
        let intent = IntentExtractor::new().extract(
            "机动规避敌方导弹的同时，释放干扰，发射侦察无人机侦察蓝方指挥所",
            &snap,
            &policy,
        );
        assert_eq!(intent.kind, MissionKind::ReactiveDefense);

        let mission = MissionCompiler::new(SelfPlatformAllocator::new()).compile(
            &intent,
            &snap,
            &PlayRegistry::bundled(),
            &policy,
            &CompileParams::default(),
        );

        assert_eq!(mission.kind, MissionKind::ReactiveDefense);
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::SetHeading { .. })));
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::ReleaseDecoy { .. })));
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::Jam { .. })));
        assert!(mission.functions.iter().any(|f| matches!(
            &f.command,
            PlatformCommandSpec::Fire { weapon_id, .. } if weapon_id == "scout_uav_slot"
        )));
        assert!(mission.functions.iter().all(|f| {
            f.preconditions
                .iter()
                .any(|precondition| precondition == "event:missile_inbound")
        }));
    }

    #[test]
    fn compiles_salvo_hint_into_fire_spec() {
        let mission = compile_text("齐射2枚打击蓝方指挥所");
        let fire = mission
            .functions
            .iter()
            .find_map(|f| match &f.command {
                PlatformCommandSpec::Fire { salvo_size, .. } => Some(salvo_size),
                _ => None,
            })
            .expect("expected fire function");
        assert_eq!(*fire, Some(2));
    }

    #[test]
    fn fire_weapon_selection_prefers_recommended_names_without_filtering() {
        let weapons = vec![
            WeaponState {
                weapon_id: "scout_uav_slot".into(),
                weapon_type: "J7_UAV_WEAPON".into(),
                quantity_remaining: 2.0,
                max_range_m: None,
                min_range_m: None,
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            },
            WeaponState {
                weapon_id: "loiter_wave1".into(),
                weapon_type: "RED_LOITER_MUN".into(),
                quantity_remaining: 16.0,
                max_range_m: None,
                min_range_m: None,
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            },
            WeaponState {
                weapon_id: "gun_30mm".into(),
                weapon_type: "30MM_BULLET".into(),
                quantity_remaining: 1500.0,
                max_range_m: None,
                min_range_m: None,
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            },
        ];

        let (weapon_id, has_weapon) = select_recommended_weapon(&weapons).unwrap();
        assert!(has_weapon);
        assert_eq!(weapon_id, "loiter_wave1");
    }

    #[test]
    fn engage_without_target_drops_fire_and_lowers_confidence() {
        // "打击" with no resolvable target → engage class, but nothing to fire at.
        let mission = compile_text("打击敌人");
        assert_eq!(mission.kind, MissionKind::Engage);
        assert!(
            !mission.has_lethal_function(),
            "no target → no fire function"
        );
        assert!(mission.confidence < 0.75);
    }

    #[test]
    fn rtb_compiles_to_goto_and_is_valid() {
        let mission = compile_text("所有无人机返航");
        assert_eq!(mission.kind, MissionKind::Rtb);
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::Goto { .. })));
        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    }

    #[test]
    fn unknown_intent_yields_unknown_mission_with_zero_confidence() {
        let mission = compile_text("今天天气不错");
        assert_eq!(mission.kind, MissionKind::Unknown);
        assert_eq!(mission.confidence, 0.0);
        assert!(mission.functions.is_empty());
    }

    #[test]
    fn resolve_track_id_maps_truth_name_to_real_track_and_drops_unknown() {
        // The snapshot holds track "blue_command_post:1" whose truth name is
        // "blue_command_post" (mirrors ArkSIM trackId "self:N" / targetName).
        let snap = snapshot();
        // Exact track id → passes through unchanged.
        assert_eq!(
            resolve_track_id(&snap, "blue_command_post:1").as_deref(),
            Some("blue_command_post:1")
        );
        // Truth name (what the LLM tends to pick) → mapped to the real track id,
        // never the bare name that makes AFSIM log `TrackID is error:...`.
        assert_eq!(
            resolve_track_id(&snap, "blue_command_post").as_deref(),
            Some("blue_command_post:1")
        );
        // No real track for this name → dropped, not fabricated.
        assert_eq!(resolve_track_id(&snap, "ghost_999"), None);
        assert_eq!(resolve_track_id(&snap, ""), None);
    }

    #[test]
    fn resolve_track_id_accepts_hostile_platform_id_when_no_sensor_track() {
        let mut snap = snapshot();
        let sam = PlatformState::minimal("blue_sam_site_1");
        snap.platforms.push(sam);
        assert_eq!(
            resolve_track_id(&snap, "blue_sam_site_1").as_deref(),
            Some("blue_sam_site_1")
        );
    }

    #[test]
    fn resolve_track_id_prefers_sensor_track_for_platform_id() {
        let mut snap = snapshot();
        snap.platforms[0].tracks.push(Track {
            track_id: "self:9".into(),
            target_name: "blue_sam_site_1".into(),
            classification: "sam_site".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((30.01, 120.01, 0.0)),
            heading_deg: Some(0.0),
            speed_ms: None,
            range_m: Some(8_000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        });
        let sam = PlatformState::minimal("blue_sam_site_1");
        snap.platforms.push(sam);
        assert_eq!(
            resolve_track_id(&snap, "blue_sam_site_1").as_deref(),
            Some("self:9")
        );
    }

    #[test]
    fn track_only_mission_has_no_lethal_function() {
        let mission = compile_text("对蓝方指挥所只跟踪，武器先别动");
        assert_eq!(mission.kind, MissionKind::Track);
        assert!(!mission.has_lethal_function());
        assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    }

    #[test]
    fn compiles_turn_left_to_set_heading_command() {
        let mission = compile_text("self 左转，速度5米每秒");
        assert!(mission.functions.iter().any(|f| {
            matches!(
                f.command,
                PlatformCommandSpec::SetHeading {
                    heading_deg,
                    speed_ms: Some(5.0),
                    turn_direction: Some(TurnDirection::Left),
                    ..
                } if (heading_deg - 270.0).abs() < 0.01
            )
        }));
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::SetSpeed { speed_ms } if (speed_ms - 5.0).abs() < 0.01)));
    }

    #[test]
    fn compiles_flank_only_with_follow_route() {
        let mission = compile_text("绕后接近蓝方指挥所，保持安全距离3公里");
        assert_eq!(mission.kind, MissionKind::Recon);
        assert!(mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::FollowRoute { .. })));
    }
}
