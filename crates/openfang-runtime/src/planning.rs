//! Slow-loop intent inbox and mission planner.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use openfang_types::cognition::{CommanderIntent, SituationAssessment};
use openfang_types::config::{PlatformControlPolicy, WeaponEmploymentMode, WeaponEmploymentRule};
use openfang_types::platform::{Affiliation, PlatformState, Track, WeaponState, WorldSnapshot};
use openfang_types::umaa::{MissionConfig, Objective, TargetAllocation};
use serde::{Deserialize, Serialize};

use openfang_types::mission_dsl::MissionKind;

use crate::intent_extractor::classify_mission_kind;
use crate::intervention::{InterventionDecision, InterventionGate, InterventionRequest};
use crate::play_registry::{PlayRegistry, PlaySelectionContext};

#[derive(Default)]
pub struct IntentInbox {
    intents: Mutex<VecDeque<CommanderIntent>>,
}

impl IntentInbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn submit(&self, intent: CommanderIntent) {
        self.intents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(intent);
    }

    pub fn pop_next(&self) -> Option<CommanderIntent> {
        self.intents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    pub fn peek_next(&self) -> Option<CommanderIntent> {
        self.intents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .front()
            .cloned()
    }

    pub fn ack_next(&self, intent_id: &str) -> Option<CommanderIntent> {
        let mut intents = self.intents.lock().unwrap_or_else(|e| e.into_inner());
        if intents
            .front()
            .map(|intent| intent.id.as_str() == intent_id)
            .unwrap_or(false)
        {
            intents.pop_front()
        } else {
            None
        }
    }

    /// Merge resolved label targets into the front intent and clear
    /// `priority_labels` so label resolution is one-shot.
    ///
    /// If the intent only had labels and none resolved, add an impossible
    /// sentinel track. That preserves "no allocation" semantics instead of
    /// letting an empty `priority_tracks` mean "all tracks".
    pub fn merge_resolved_front(
        &self,
        intent_id: &str,
        resolved: &[String],
    ) -> Option<CommanderIntent> {
        let mut intents = self.intents.lock().unwrap_or_else(|e| e.into_inner());
        let intent = intents.front_mut()?;
        if intent.id != intent_id {
            return None;
        }
        let mut seen: HashSet<String> = intent.priority_tracks.iter().cloned().collect();
        for track_id in resolved.iter().filter(|id| !id.trim().is_empty()) {
            if seen.insert(track_id.clone()) {
                intent.priority_tracks.push(track_id.clone());
            }
        }
        if intent.priority_tracks.is_empty() {
            intent
                .priority_tracks
                .push("__unresolved_priority_label__".into());
        }
        intent.priority_labels.clear();
        Some(intent.clone())
    }

    /// Snapshot the queued intents without consuming them (for UI display).
    pub fn list(&self) -> Vec<CommanderIntent> {
        self.intents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.intents.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelResolutionState {
    Pending,
    Applied,
    Dismissed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelResolution {
    pub label: String,
    pub candidates: Vec<LabelCandidateMatch>,
    pub selected_track_id: Option<String>,
}

impl LabelResolution {
    pub fn selected_track_ids(resolutions: &[Self]) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        for id in resolutions
            .iter()
            .filter_map(|resolution| resolution.selected_track_id.as_ref())
        {
            if seen.insert(id.clone()) {
                ids.push(id.clone());
            }
        }
        ids
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelCandidateMatch {
    pub track_id: String,
    pub source_platform_name: Option<String>,
    pub platform_type: Option<String>,
    pub track_classification: String,
    pub affiliation: Affiliation,
    pub weapon_reachable: bool,
    pub score: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLabelResolution {
    pub id: String,
    pub intent_id: String,
    pub objective: String,
    pub resolutions: Vec<LabelResolution>,
    pub created_at: f64,
    pub state: LabelResolutionState,
}

impl PendingLabelResolution {
    pub fn selected_track_ids(&self) -> Vec<String> {
        LabelResolution::selected_track_ids(&self.resolutions)
    }
}

#[derive(Default)]
pub struct LabelResolutionRegistry {
    resolutions: Mutex<Vec<PendingLabelResolution>>,
}

impl LabelResolutionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn submit(
        &self,
        intent: &CommanderIntent,
        resolutions: Vec<LabelResolution>,
        created_at: f64,
    ) -> PendingLabelResolution {
        let id = label_resolution_id(intent);
        let mut pending = self.resolutions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = pending
            .iter()
            .find(|resolution| {
                resolution.id == id && resolution.state == LabelResolutionState::Pending
            })
            .cloned()
        {
            return existing;
        }
        let resolution = PendingLabelResolution {
            id,
            intent_id: intent.id.clone(),
            objective: intent.objective.clone(),
            resolutions,
            created_at,
            state: LabelResolutionState::Pending,
        };
        pending.push(resolution.clone());
        resolution
    }

    pub fn list_pending(&self) -> Vec<PendingLabelResolution> {
        self.resolutions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|resolution| resolution.state == LabelResolutionState::Pending)
            .cloned()
            .collect()
    }

    pub fn get_pending(&self, id: &str) -> Option<PendingLabelResolution> {
        self.resolutions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|resolution| {
                resolution.id == id && resolution.state == LabelResolutionState::Pending
            })
            .cloned()
    }

    pub fn mark_applied(&self, id: &str) -> Option<PendingLabelResolution> {
        self.mark(id, LabelResolutionState::Applied)
    }

    pub fn dismiss(&self, id: &str) -> Option<PendingLabelResolution> {
        self.mark(id, LabelResolutionState::Dismissed)
    }

    fn mark(&self, id: &str, state: LabelResolutionState) -> Option<PendingLabelResolution> {
        let mut resolutions = self.resolutions.lock().unwrap_or_else(|e| e.into_inner());
        let resolution = resolutions
            .iter_mut()
            .find(|resolution| resolution.id == id)?;
        resolution.state = state;
        Some(resolution.clone())
    }

    pub fn has_pending_for_intent(&self, intent_id: &str) -> bool {
        self.resolutions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|resolution| {
                resolution.intent_id == intent_id
                    && resolution.state == LabelResolutionState::Pending
            })
    }
}

fn label_resolution_id(intent: &CommanderIntent) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    intent.id.hash(&mut hasher);
    for label in &intent.priority_labels {
        normalize_label(label).hash(&mut hasher);
    }
    format!("label-resolution:{:016x}", hasher.finish())
}

pub struct LabelResolveContext<'a> {
    pub snapshot: &'a WorldSnapshot,
    pub labels: &'a [String],
    pub control_policy: &'a PlatformControlPolicy,
}

#[derive(Debug, Clone)]
pub struct DeterministicLabelResolver {
    min_score: f64,
    max_candidates: usize,
}

impl Default for DeterministicLabelResolver {
    fn default() -> Self {
        Self {
            min_score: 35.0,
            max_candidates: 5,
        }
    }
}

impl DeterministicLabelResolver {
    pub fn resolve(&self, ctx: LabelResolveContext<'_>) -> Vec<LabelResolution> {
        let candidates = build_label_candidates(ctx.snapshot, ctx.control_policy);
        ctx.labels
            .iter()
            .filter(|label| !label.trim().is_empty())
            .map(|label| self.resolve_one(label, &candidates, ctx.control_policy))
            .collect()
    }

    fn resolve_one(
        &self,
        label: &str,
        candidates: &[CandidateTrack],
        policy: &PlatformControlPolicy,
    ) -> LabelResolution {
        let mut scored: Vec<LabelCandidateMatch> = candidates
            .iter()
            .map(|candidate| score_candidate(label, candidate, policy))
            .filter(|candidate| candidate.score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.track_id.cmp(&b.track_id))
        });
        scored.truncate(self.max_candidates);
        let selected_track_id = scored
            .first()
            .filter(|candidate| candidate.score >= self.min_score)
            .map(|candidate| candidate.track_id.clone());
        LabelResolution {
            label: label.to_string(),
            candidates: scored,
            selected_track_id,
        }
    }
}

#[derive(Debug, Clone)]
struct CandidateTrack {
    track_id: String,
    track_id_prefix: Option<String>,
    source_platform_name: Option<String>,
    platform_type: Option<String>,
    track_classification: String,
    affiliation: Affiliation,
    iff: String,
    weapon_reachable: bool,
}

fn build_label_candidates(
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
) -> Vec<CandidateTrack> {
    let own_platforms: Vec<&PlatformState> = snapshot
        .platforms
        .iter()
        .filter(|platform| {
            policy.controlled_side.matches(platform.affiliation)
                && (policy.controlled_platforms.is_empty()
                    || policy
                        .controlled_platforms
                        .iter()
                        .any(|id| id == &platform.id))
        })
        .collect();
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for observer in &snapshot.platforms {
        for track in &observer.tracks {
            if !seen.insert(track.track_id.clone()) {
                continue;
            }
            let prefix = track
                .track_id
                .split_once(':')
                .map(|(prefix, _)| prefix.to_string());
            let source = prefix
                .as_deref()
                .and_then(|id| find_platform(snapshot, id))
                .or_else(|| find_platform(snapshot, &track.track_id));
            candidates.push(CandidateTrack {
                track_id: track.track_id.clone(),
                track_id_prefix: prefix,
                source_platform_name: source
                    .map(|platform| platform.name.clone())
                    .or_else(|| Some(observer.name.clone())),
                platform_type: source.map(|platform| platform.platform_type.clone()),
                track_classification: track.classification.clone(),
                affiliation: track.affiliation,
                iff: track.iff.clone(),
                weapon_reachable: own_platforms
                    .iter()
                    .any(|platform| platform_can_reach_track(platform, track)),
            });
        }
    }
    for platform in &snapshot.platforms {
        if !seen.insert(platform.id.clone()) || policy.controlled_side.matches(platform.affiliation)
        {
            continue;
        }
        candidates.push(CandidateTrack {
            track_id: platform.id.clone(),
            track_id_prefix: Some(platform.id.clone()),
            source_platform_name: Some(platform.name.clone()),
            platform_type: Some(platform.platform_type.clone()),
            track_classification: platform.platform_type.clone(),
            affiliation: platform.affiliation,
            iff: "unknown".into(),
            weapon_reachable: own_platforms
                .iter()
                .any(|own| platform_weapon_can_reach_platform(own, platform)),
        });
    }
    candidates
}

fn find_platform<'a>(snapshot: &'a WorldSnapshot, id_or_name: &str) -> Option<&'a PlatformState> {
    snapshot
        .platforms
        .iter()
        .find(|platform| platform.id == id_or_name || platform.name == id_or_name)
}

fn platform_can_reach_track(platform: &PlatformState, track: &Track) -> bool {
    platform.onboard_weapons.iter().any(|weapon| {
        weapon.is_ready
            && weapon.quantity_remaining > 0.0
            && track
                .range_m
                .map(|range| weapon_can_reach(weapon, range))
                .unwrap_or(false)
    })
}

fn platform_weapon_can_reach_platform(platform: &PlatformState, target: &PlatformState) -> bool {
    let range_m = platform_distance_m(platform, target);
    platform.onboard_weapons.iter().any(|weapon| {
        weapon.is_ready && weapon.quantity_remaining > 0.0 && weapon_can_reach(weapon, range_m)
    })
}

fn platform_distance_m(a: &PlatformState, b: &PlatformState) -> f64 {
    let dlat = (b.pose.lat_deg - a.pose.lat_deg).to_radians();
    let dlon = (b.pose.lon_deg - a.pose.lon_deg).to_radians();
    let mean_lat = ((a.pose.lat_deg + b.pose.lat_deg) * 0.5).to_radians();
    let north_m = dlat * 6_371_000.0;
    let east_m = dlon * 6_371_000.0 * mean_lat.cos();
    let up_m = b.pose.alt_m - a.pose.alt_m;
    (north_m * north_m + east_m * east_m + up_m * up_m).sqrt()
}

fn weapon_can_reach(weapon: &WeaponState, range_m: f64) -> bool {
    if !range_m.is_finite() {
        return false;
    }
    let min = weapon.min_range_m.unwrap_or(0.0);
    let max = weapon.max_range_m.unwrap_or(f64::INFINITY);
    range_m >= min && range_m <= max
}

fn score_candidate(
    label: &str,
    candidate: &CandidateTrack,
    policy: &PlatformControlPolicy,
) -> LabelCandidateMatch {
    let normalized = normalize_label(label);
    let mut score = 0.0;
    let mut reasons = Vec::new();
    add_match_score(
        &mut score,
        &mut reasons,
        "track_id",
        &normalized,
        &candidate.track_id,
        100.0,
    );
    if let Some(prefix) = candidate.track_id_prefix.as_deref() {
        add_match_score(
            &mut score,
            &mut reasons,
            "track_prefix",
            &normalized,
            prefix,
            75.0,
        );
    }
    if let Some(name) = candidate.source_platform_name.as_deref() {
        add_match_score(
            &mut score,
            &mut reasons,
            "platform_name",
            &normalized,
            name,
            70.0,
        );
    }
    if let Some(platform_type) = candidate.platform_type.as_deref() {
        add_match_score(
            &mut score,
            &mut reasons,
            "platform_type",
            &normalized,
            platform_type,
            45.0,
        );
    }
    add_match_score(
        &mut score,
        &mut reasons,
        "classification",
        &normalized,
        &candidate.track_classification,
        45.0,
    );
    for keyword in label_type_keywords(&normalized) {
        let platform_type = candidate
            .platform_type
            .as_deref()
            .map(normalize_label)
            .unwrap_or_default();
        let classification = normalize_label(&candidate.track_classification);
        if platform_type.contains(keyword) || classification.contains(keyword) {
            score += 25.0;
            reasons.push(format!("type keyword '{keyword}'"));
        }
    }
    if let Some(expected_side) = label_side(&normalized) {
        let side_matches = if expected_side == Affiliation::Foe {
            policy.track_is_threat(candidate.affiliation, &candidate.iff)
        } else {
            expected_side == candidate.affiliation
        };
        if side_matches {
            score += 20.0;
            reasons.push(format!("side matches {:?}", candidate.affiliation));
        } else {
            score -= 45.0;
            reasons.push(format!(
                "side mismatch: label expects {:?}, candidate is {:?}",
                expected_side, candidate.affiliation
            ));
        }
    }
    if policy.track_is_threat(candidate.affiliation, &candidate.iff) {
        score += 8.0;
        reasons.push("candidate is threat under control policy".into());
    } else if policy.controlled_side.matches(candidate.affiliation) {
        score -= 40.0;
        reasons.push("candidate is friendly/own-side protected".into());
    }
    if candidate.weapon_reachable {
        score += 10.0;
        reasons.push("reachable by ready own-force weapon".into());
    }
    LabelCandidateMatch {
        track_id: candidate.track_id.clone(),
        source_platform_name: candidate.source_platform_name.clone(),
        platform_type: candidate.platform_type.clone(),
        track_classification: candidate.track_classification.clone(),
        affiliation: candidate.affiliation,
        weapon_reachable: candidate.weapon_reachable,
        score: score.max(0.0),
        reasons,
    }
}

fn add_match_score(
    score: &mut f64,
    reasons: &mut Vec<String>,
    field: &str,
    label: &str,
    value: &str,
    max_score: f64,
) {
    let value = normalize_label(value);
    if value.is_empty() {
        return;
    }
    if label == value {
        *score += max_score;
        reasons.push(format!("{field} exact match"));
    } else if label.contains(&value) || value.contains(label) {
        *score += max_score * 0.8;
        reasons.push(format!("{field} contains match"));
    } else {
        let similarity = char_overlap(label, &value);
        if similarity >= 0.45 {
            *score += max_score * similarity * 0.6;
            reasons.push(format!("{field} fuzzy {:.2}", similarity));
        }
    }
}

fn normalize_label(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || is_cjk(*c))
        .collect()
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

fn char_overlap(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let a_chars: HashSet<char> = a.chars().collect();
    let b_chars: HashSet<char> = b.chars().collect();
    let intersection = a_chars.intersection(&b_chars).count() as f64;
    let denominator = a_chars.len().max(b_chars.len()) as f64;
    intersection / denominator
}

fn label_side(label: &str) -> Option<Affiliation> {
    if label.contains("蓝") || label.contains("blue") {
        Some(Affiliation::Blue)
    } else if label.contains("红") || label.contains("red") {
        Some(Affiliation::Red)
    } else if label.contains("敌") || label.contains("foe") || label.contains("hostile") {
        Some(Affiliation::Foe)
    } else if label.contains("友") || label.contains("friend") {
        Some(Affiliation::Friend)
    } else {
        None
    }
}

fn label_type_keywords(label: &str) -> Vec<&'static str> {
    let mut keywords = Vec::new();
    if label.contains("指挥")
        || label.contains("司令")
        || label.contains("command")
        || label.contains("hq")
    {
        keywords.extend(["command", "commandpost", "headquarters", "hq"]);
    }
    if label.contains("导弹") || label.contains("missile") {
        keywords.push("missile");
    }
    if label.contains("无人机") || label.contains("uav") || label.contains("drone") {
        keywords.extend(["uav", "drone"]);
    }
    if label.contains("舰")
        || label.contains("艇")
        || label.contains("ship")
        || label.contains("vessel")
    {
        keywords.extend(["ship", "vessel", "boat"]);
    }
    keywords
}

pub struct Planner {
    intervention: Arc<RwLock<InterventionGate>>,
    /// Optional allow-list of platform ids the slow loop may task. Empty means
    /// "no id restriction" within the configured [`PlatformConfig::controlled_side`].
    controlled_platforms: Vec<String>,
    /// Tactical style (play) library loaded from the bundled
    /// `tactical_workflows.toml`. Drives `MissionConfig.play_name` selection in
    /// `baseline`. Empty registry ⇒ play_name stays `None` (legacy fallback).
    play_registry: PlayRegistry,
}

impl Planner {
    pub fn new(intervention: Arc<RwLock<InterventionGate>>) -> Self {
        Self {
            intervention,
            controlled_platforms: Vec::new(),
            play_registry: PlayRegistry::bundled(),
        }
    }

    /// Restrict slow-loop tasking to the given platform ids (empty = no limit).
    pub fn set_controlled_platforms(&mut self, platforms: Vec<String>) {
        self.controlled_platforms = platforms;
    }

    /// Compute the deterministic baseline mission (phase/objectives/allocations)
    /// from the assessment and optional commander intent — without running the
    /// human-intervention checkpoint. This is the safe, reproducible core that
    /// any optional LLM refinement may only *narrow*, never extend.
    pub fn baseline(
        &self,
        assessment: &SituationAssessment,
        intent: Option<&CommanderIntent>,
        mut mission: MissionConfig,
    ) -> MissionConfig {
        let mut allocations = allocations_for(assessment, intent);
        if !self.controlled_platforms.is_empty() {
            allocations.retain(|alloc| {
                self.controlled_platforms
                    .iter()
                    .any(|id| id == &alloc.platform_id)
            });
        }
        let phase = if allocations.is_empty() {
            "patrol"
        } else {
            "engage"
        };
        mission.phase = Some(phase.to_string());
        mission.objectives = vec![Objective {
            id: format!("{}:{phase}:{}", mission.mission_id, assessment.timestamp),
            description: intent
                .map(|intent| intent.objective.clone())
                .unwrap_or_else(|| format!("{phase} based on current situation assessment")),
            priority: if phase == "engage" { 100 } else { 10 },
            status: "pending".into(),
        }];
        mission.target_track_id = allocations
            .first()
            .map(|allocation| allocation.track_id.clone())
            .or_else(|| {
                assessment
                    .threats
                    .first()
                    .map(|threat| threat.track_id.clone())
            });
        mission.play_name = self.select_play(assessment, intent);
        mission.allocations = allocations;
        mission
    }

    /// Choose a tactical style (play) for this cycle. The mission kind is taken
    /// from the commander intent objective (deterministic keyword classifier);
    /// when no intent is present it is inferred from the threat picture (an
    /// engage opportunity ⇒ `Engage`, otherwise `Patrol`). The
    /// [`PlayRegistry`] then filters candidate plays by precondition / ROI /
    /// risk and the highest-preference survivor wins. Returns `None` when the
    /// registry is empty or no play passes the gates (legacy fallback path).
    fn select_play(
        &self,
        assessment: &SituationAssessment,
        intent: Option<&CommanderIntent>,
    ) -> Option<String> {
        if self.play_registry.is_empty() {
            return None;
        }
        let has_target = !assessment.threats.is_empty() || !assessment.opportunities.is_empty();
        let kind = match intent {
            Some(intent) => {
                let inferred = classify_mission_kind(&intent.objective);
                if inferred == MissionKind::Unknown {
                    if has_target {
                        MissionKind::Engage
                    } else {
                        MissionKind::Patrol
                    }
                } else {
                    inferred
                }
            }
            None if has_target => MissionKind::Engage,
            None => MissionKind::Patrol,
        };
        let controlled_count = if self.controlled_platforms.is_empty() {
            assessment.own_force.total_platforms
        } else {
            self.controlled_platforms.len()
        };
        let has_sensor = controlled_count > 0;
        let has_weapon = !assessment.opportunities.is_empty()
            || self.controlled_platforms.iter().any(|id| {
                let id = id.to_ascii_lowercase();
                id.contains("usv")
                    || id.contains("self")
                    || id.contains("gun")
                    || id.contains("cannon")
                    || id.contains("cca")
            });
        let ctx = PlaySelectionContext {
            has_weapon,
            has_sensor,
            has_target,
            pid_or_designated: has_target,
            controlled_platform_count: controlled_count,
            ..PlaySelectionContext::new()
        };
        self.play_registry
            .select(kind, &ctx)
            .first()
            .map(|play| play.name.clone())
    }

    /// Run the `mission_approval` intervention checkpoint against a (possibly
    /// refined) mission and classify the planning outcome.
    pub fn gate(&self, mission: MissionConfig) -> PlanningOutcome {
        let platform_id = mission
            .allocations
            .first()
            .map(|allocation| allocation.platform_id.as_str())
            .unwrap_or("mission");
        let fingerprint = plan_fingerprint(&mission);
        let decision = {
            let gate = self.intervention.read().unwrap_or_else(|e| e.into_inner());
            gate.evaluate(InterventionRequest {
                stage: "mission_approval",
                platform_id,
                command_class: None,
                source: None,
                track_id: mission
                    .allocations
                    .first()
                    .map(|allocation| allocation.track_id.as_str()),
                intent_id: &mission.mission_id,
                weapon_release_authority: None,
                plan_fingerprint: Some(&fingerprint),
            })
        };
        match decision {
            InterventionDecision::Pass | InterventionDecision::RoeDriven => {
                PlanningOutcome::Approved(mission)
            }
            InterventionDecision::Deny(reason) => PlanningOutcome::Denied { reason, mission },
            InterventionDecision::Pending { approval_id, .. } => PlanningOutcome::Pending {
                approval_id,
                mission,
            },
        }
    }

    pub fn plan(
        &self,
        assessment: &SituationAssessment,
        intent: Option<CommanderIntent>,
        mission: MissionConfig,
    ) -> PlanningOutcome {
        let baseline = self.baseline(assessment, intent.as_ref(), mission);
        self.gate(baseline)
    }
}

/// Compute a stable content fingerprint for a mission plan. The fingerprint
/// covers the mission id, phase, objectives, and the sorted set of target
/// allocations so that **any change to operator-visible plan content yields a
/// new fingerprint** — forcing a fresh human approval rather than letting a
/// stale approval release a plan that has changed in response to the evolving
/// situation.
pub fn plan_fingerprint(mission: &MissionConfig) -> String {
    use sha2::{Digest, Sha256};

    let mut allocations: Vec<String> = mission
        .allocations
        .iter()
        .map(|a| format!("{}|{}|{}", a.platform_id, a.weapon_id, a.track_id))
        .collect();
    allocations.sort();

    let mut hasher = Sha256::new();
    hasher.update(mission.mission_id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(mission.play_name.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\x1f");
    hasher.update(mission.target_track_id.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\x1f");
    hasher.update(mission.phase.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\x1f");
    for objective in &mission.objectives {
        hasher.update(objective.id.as_bytes());
        hasher.update(b"\x1d");
        hasher.update(objective.description.as_bytes());
        hasher.update(b"\x1d");
        hasher.update(objective.priority.to_le_bytes());
        hasher.update(b"\x1d");
        hasher.update(objective.status.as_bytes());
        hasher.update(b"\x1e");
    }
    hasher.update(b"\x1f");
    for alloc in &allocations {
        hasher.update(alloc.as_bytes());
        hasher.update(b"\x1e");
    }
    let digest = hasher.finalize();
    // 16 hex chars (64 bits) is ample for collision resistance here.
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Owned context handed to a [`MissionPlanRefiner`]. Owned (not borrowed) so it
/// can cross `.await` points inside a spawned slow-loop task.
#[derive(Debug, Clone)]
pub struct RefineContext {
    pub assessment: SituationAssessment,
    pub intent: Option<CommanderIntent>,
    /// Rule-derived baseline mission. Its `allocations` are the ONLY engagement
    /// options a refiner may select from.
    pub baseline: MissionConfig,
}

/// Max rounds the brain may request in a single salvo. A hard ceiling so an
/// LLM cannot dump a magazine; the EngagementGuard re-checks live ammo anyway.
pub const MAX_SALVO_SIZE: u32 = 8;

/// A per-allocation weapon-employment override the brain may attach to a kept
/// allocation. `index` refers to `baseline.allocations`.
#[derive(Debug, Clone, Deserialize)]
pub struct SalvoOverride {
    pub index: usize,
    pub salvo_size: u32,
}

/// A constrained refinement an LLM (or any external planner) may return — the
/// bounded slow-loop *policy* object (target selection, fire-authorization
/// proposals, weapon employment).
///
/// SAFETY: every field is advisory and bounded. `selected_indices` can only
/// *narrow* the baseline allocations; `phase` is clamped to a known set;
/// `authorize_indices` can only name allocations the rule layer already
/// produced; `salvo_overrides` are clamped to [`MAX_SALVO_SIZE`]. The refiner
/// can never add a target the rule layer did not validate, never writes an
/// authorization itself (the kernel does, ROE-gated), and never bypasses the
/// CommandGate.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlanRefinement {
    /// Indices into `baseline.allocations` to keep. Empty = keep all baseline.
    #[serde(default)]
    pub selected_indices: Vec<usize>,
    /// Optional phase override, clamped to `patrol|engage|track|rtb`.
    #[serde(default)]
    pub phase: Option<String>,
    /// Optional human-readable objective override for the primary objective.
    #[serde(default)]
    pub objective: Option<String>,
    /// Indices into `baseline.allocations` the brain proposes to AUTHORIZE for
    /// fire. Advisory only: the kernel decides whether to actually write the
    /// authorization (config-gated, weapons-free only); the gate/ROE still rule.
    #[serde(default)]
    pub authorize_indices: Vec<usize>,
    /// Per-allocation salvo employment overrides (clamped on apply).
    #[serde(default)]
    pub salvo_overrides: Vec<SalvoOverride>,
}

/// A fire-authorization the brain proposes, resolved to a concrete
/// `(platform, track)`. The kernel decides whether to honor it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuthorizationProposal {
    pub platform_id: String,
    pub track_id: String,
}

/// Resolve the brain's `authorize_indices` against the baseline allocations,
/// keeping only those that survive `selected_indices` narrowing (you can only
/// authorize what you are still committing to fire). Pure — no side effects.
pub fn authorization_proposals(
    baseline: &MissionConfig,
    refinement: &PlanRefinement,
) -> Vec<AuthorizationProposal> {
    let kept: std::collections::HashSet<usize> = if refinement.selected_indices.is_empty() {
        (0..baseline.allocations.len()).collect()
    } else {
        refinement
            .selected_indices
            .iter()
            .copied()
            .filter(|idx| *idx < baseline.allocations.len())
            .collect()
    };
    let mut seen = std::collections::HashSet::new();
    refinement
        .authorize_indices
        .iter()
        .copied()
        .filter(|idx| kept.contains(idx))
        .filter(|idx| seen.insert(*idx))
        .filter_map(|idx| baseline.allocations.get(idx))
        .map(|alloc| AuthorizationProposal {
            platform_id: alloc.platform_id.clone(),
            track_id: alloc.track_id.clone(),
        })
        .collect()
}

/// Apply a refinement to a baseline mission under hard guardrails.
pub fn apply_refinement(mut mission: MissionConfig, refinement: &PlanRefinement) -> MissionConfig {
    apply_refinement_with_policy(mission, refinement, &PlatformControlPolicy::default(), None)
}

/// Apply a refinement, then clamp weapon employment through TOML policy.
pub fn apply_refinement_with_policy(
    mut mission: MissionConfig,
    refinement: &PlanRefinement,
    policy: &PlatformControlPolicy,
    snapshot: Option<&WorldSnapshot>,
) -> MissionConfig {
    // Weapon employment: bake clamped salvo overrides onto baseline allocations
    // by index BEFORE narrowing (the override indices reference the baseline).
    for ovr in &refinement.salvo_overrides {
        if let Some(alloc) = mission.allocations.get_mut(ovr.index) {
            let size = ovr.salvo_size.clamp(1, MAX_SALVO_SIZE);
            alloc.salvo_size = Some(size);
            alloc.weapon_policy = Some(if size > 1 { "salvo" } else { "single" }.to_string());
        }
    }
    apply_weapon_employment_policy(&mut mission, policy, snapshot);
    if !refinement.selected_indices.is_empty() {
        let mut seen = std::collections::HashSet::new();
        let narrowed: Vec<TargetAllocation> = refinement
            .selected_indices
            .iter()
            .filter(|idx| seen.insert(**idx))
            .filter_map(|&idx| mission.allocations.get(idx).cloned())
            .collect();
        // Only accept the narrowing if it kept at least one valid allocation;
        // otherwise the refiner is ignored and the baseline stands.
        if !narrowed.is_empty() {
            mission.allocations = narrowed;
            let phase = if mission.allocations.is_empty() {
                "patrol"
            } else {
                "engage"
            };
            mission.phase = Some(phase.to_string());
        }
    }
    if let Some(phase) = refinement.phase.as_deref() {
        if matches!(phase, "patrol" | "engage" | "track" | "rtb") {
            mission.phase = Some(phase.to_string());
        }
    }
    if let Some(objective) = refinement.objective.as_ref() {
        if let Some(first) = mission.objectives.first_mut() {
            first.description = objective.clone();
        }
    }
    mission
}

fn apply_weapon_employment_policy(
    mission: &mut MissionConfig,
    policy: &PlatformControlPolicy,
    snapshot: Option<&WorldSnapshot>,
) {
    for alloc in &mut mission.allocations {
        let Some(rule) = employment_rule_for(alloc, policy, snapshot) else {
            continue;
        };
        let cap = rule
            .max_salvo_size
            .unwrap_or(MAX_SALVO_SIZE)
            .clamp(1, MAX_SALVO_SIZE);
        match rule.mode {
            WeaponEmploymentMode::Single => {
                alloc.salvo_size = Some(1);
                alloc.weapon_policy = Some("single".into());
            }
            WeaponEmploymentMode::Salvo => {
                let requested = rule.salvo_size.or(alloc.salvo_size).unwrap_or(2);
                let size = requested.clamp(2, cap.max(2));
                alloc.salvo_size = Some(size);
                alloc.weapon_policy = Some("salvo".into());
            }
            WeaponEmploymentMode::Llm => {
                if let Some(size) = alloc.salvo_size {
                    let size = size.clamp(1, cap);
                    alloc.salvo_size = Some(size);
                    alloc.weapon_policy = Some(if size > 1 { "salvo" } else { "single" }.into());
                }
            }
        }
    }
}

fn employment_rule_for<'a>(
    alloc: &TargetAllocation,
    policy: &'a PlatformControlPolicy,
    snapshot: Option<&WorldSnapshot>,
) -> Option<&'a WeaponEmploymentRule> {
    let weapon_id = alloc.weapon_id.to_ascii_lowercase();
    if let Some(rule) = policy.weapon_employment.get(&weapon_id) {
        return Some(rule);
    }

    if let Some(weapon_type) = weapon_type_for_alloc(alloc, snapshot) {
        let type_key = weapon_type.to_ascii_lowercase();
        if let Some(rule) = policy.weapon_employment.get(&type_key) {
            return Some(rule);
        }
        if let Some(category) = weapon_category(&type_key) {
            if let Some(rule) = policy.weapon_employment.get(category) {
                return Some(rule);
            }
        }
    }

    weapon_category(&weapon_id).and_then(|category| policy.weapon_employment.get(category))
}

fn weapon_type_for_alloc(
    alloc: &TargetAllocation,
    snapshot: Option<&WorldSnapshot>,
) -> Option<String> {
    snapshot?
        .platforms
        .iter()
        .find(|platform| platform.id == alloc.platform_id || alloc.platform_id == "self")
        .and_then(|platform| {
            platform
                .onboard_weapons
                .iter()
                .find(|weapon| weapon.weapon_id == alloc.weapon_id)
        })
        .map(|weapon| weapon.weapon_type.clone())
}

fn weapon_category(value: &str) -> Option<&'static str> {
    if value.contains("gun") || value.contains("cannon") || value.contains("炮") {
        Some("gun")
    } else if value.contains("loiter") || value.contains("巡飞") {
        Some("loiter")
    } else if value.contains("missile") || value.contains("rocket") || value.contains("导弹") {
        Some("missile")
    } else if value.contains("torpedo") || value.contains("鱼雷") {
        Some("torpedo")
    } else {
        None
    }
}

/// Pluggable refiner that may re-prioritize a baseline mission. Implemented by
/// the kernel with an LLM backend; tests use deterministic fakes.
#[async_trait]
pub trait MissionPlanRefiner: Send + Sync {
    async fn refine(&self, ctx: RefineContext) -> Option<PlanRefinement>;
}

#[derive(Debug, Clone)]
pub enum PlanningOutcome {
    Approved(MissionConfig),
    Pending {
        approval_id: String,
        mission: MissionConfig,
    },
    Denied {
        reason: String,
        mission: MissionConfig,
    },
}

impl PlanningOutcome {
    pub fn approved(self) -> Option<MissionConfig> {
        match self {
            Self::Approved(mission) => Some(mission),
            _ => None,
        }
    }
}

fn allocations_for(
    assessment: &SituationAssessment,
    intent: Option<&CommanderIntent>,
) -> Vec<TargetAllocation> {
    // Engagement recommendation: per target, pick the SINGLE best weapon rather
    // than the full weapon×threat cross-product. Cognition surfaces every
    // fireable weapon ranked by `estimated_p_hit` (which already folds in weapon
    // characteristics — readiness, range envelope, threat score); the planner
    // commits one recommended weapon-target pairing. This avoids volleying two
    // weapons at the same track and keeps the standing plan to one fire per
    // target. First-seen target order is preserved for deterministic plans.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut best: HashMap<(String, String), &openfang_types::cognition::EngageOpportunity> =
        HashMap::new();
    for opportunity in assessment.opportunities.iter().filter(|opportunity| {
        intent
            .map(|intent| {
                intent.priority_tracks.is_empty()
                    || intent.priority_tracks.contains(&opportunity.track_id)
            })
            .unwrap_or(true)
    }) {
        let key = (
            opportunity.platform_id.clone(),
            opportunity.track_id.clone(),
        );
        match best.get(&key) {
            Some(current) if current.estimated_p_hit >= opportunity.estimated_p_hit => {}
            Some(_) => {
                best.insert(key, opportunity);
            }
            None => {
                order.push(key.clone());
                best.insert(key, opportunity);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|key| {
            best.get(&key).map(|opportunity| TargetAllocation {
                platform_id: opportunity.platform_id.clone(),
                weapon_id: opportunity.weapon_id.clone(),
                track_id: opportunity.track_id.clone(),
                allocated_at: assessment.timestamp,
                // Rule baseline employs a single round; the brain may upgrade to a
                // salvo via a refinement override (bounded + gate-checked).
                salvo_size: None,
                weapon_policy: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::{
        ControlledSide, ThreatSide, WeaponEmploymentMode, WeaponEmploymentRule,
    };
    use openfang_types::platform::{
        Domain, FuelStatus, PlatformState, Pose, Velocity, WeaponState, WorldSnapshot,
    };

    fn intent(labels: Vec<&str>, tracks: Vec<&str>) -> CommanderIntent {
        CommanderIntent {
            id: "intent-1".into(),
            issued_at: 1.0,
            issued_by: "operator".into(),
            objective: "engage semantic target".into(),
            priority_tracks: tracks.into_iter().map(String::from).collect(),
            priority_labels: labels.into_iter().map(String::from).collect(),
            constraints: vec![],
            roe_pref: None,
            cost_policy: Default::default(),
            time_windows: vec![],
            allow_degrade: false,
        }
    }

    fn platform(id: &str, affiliation: Affiliation, platform_type: &str) -> PlatformState {
        PlatformState {
            id: id.into(),
            name: id.into(),
            platform_type: platform_type.into(),
            affiliation,
            domain: Domain::Surface,
            pose: Pose {
                lat_deg: 0.0,
                lon_deg: 0.0,
                alt_m: 0.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 0.0,
                vertical_rate_ms: 0.0,
                course_deg: 0.0,
            },
            fuel: FuelStatus {
                remaining_kg: 100.0,
                max_kg: 100.0,
                consumption_rate_kg_s: 0.0,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![],
            onboard_weapons: vec![],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        }
    }

    fn track(id: &str, affiliation: Affiliation, classification: &str, range_m: f64) -> Track {
        Track {
            track_id: id.into(),
            target_name: String::new(),
            classification: classification.into(),
            affiliation,
            iff: "unknown".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m: Some(range_m),
            bearing_deg: None,
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        }
    }

    fn weapon(id: &str, max_range_m: f64) -> WeaponState {
        WeaponState {
            weapon_id: id.into(),
            weapon_type: "missile".into(),
            quantity_remaining: 1.0,
            max_range_m: Some(max_range_m),
            min_range_m: Some(0.0),
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        }
    }

    fn policy() -> PlatformControlPolicy {
        PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            threat_side: ThreatSide::Opposite,
            controlled_platforms: vec!["red_shooter".into()],
            own_platform_id: "red_shooter".into(),
            controller_id: "operator".into(),
            ..Default::default()
        }
    }

    fn snapshot() -> WorldSnapshot {
        let mut red = platform("red_shooter", Affiliation::Red, "usv");
        red.onboard_weapons.push(weapon("w1", 20_000.0));
        red.tracks.push(track(
            "blue_command_post:1",
            Affiliation::Blue,
            "command_post",
            5_000.0,
        ));

        let blue = platform("blue_command_post", Affiliation::Blue, "command_post");
        let mut red_decoy = platform("red_command_post", Affiliation::Red, "command_post");
        red_decoy.tracks.push(track(
            "red_command_post:1",
            Affiliation::Red,
            "command_post",
            3_000.0,
        ));

        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![red, blue, red_decoy],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    #[test]
    fn merge_resolved_front_deduplicates_and_clears_labels() {
        let inbox = IntentInbox::new();
        inbox.submit(intent(vec!["蓝方指挥所"], vec!["trk-1"]));

        let merged = inbox
            .merge_resolved_front("intent-1", &["trk-1".into(), "trk-2".into()])
            .unwrap();

        assert_eq!(merged.priority_tracks, vec!["trk-1", "trk-2"]);
        assert!(merged.priority_labels.is_empty());
    }

    #[test]
    fn merge_resolved_front_preserves_no_allocation_when_labels_do_not_resolve() {
        let inbox = IntentInbox::new();
        inbox.submit(intent(vec!["不存在目标"], vec![]));

        let merged = inbox.merge_resolved_front("intent-1", &[]).unwrap();

        assert_eq!(
            merged.priority_tracks,
            vec!["__unresolved_priority_label__"]
        );
        assert!(merged.priority_labels.is_empty());
    }

    #[test]
    fn deterministic_label_resolver_prefers_named_blue_command_post() {
        let resolver = DeterministicLabelResolver::default();
        let snap = snapshot();
        let policy = policy();
        let resolutions = resolver.resolve(LabelResolveContext {
            snapshot: &snap,
            labels: &["蓝方指挥所".into()],
            control_policy: &policy,
        });

        assert_eq!(
            resolutions[0].selected_track_id.as_deref(),
            Some("blue_command_post")
        );
        assert!(
            resolutions[0].candidates[0].weapon_reachable,
            "ready own weapon should contribute reachability"
        );
    }

    #[test]
    fn deterministic_label_resolver_treats_enemy_as_policy_threat_side() {
        let resolver = DeterministicLabelResolver::default();
        let mut snap = snapshot();
        snap.platforms[0].tracks.push(track(
            "blue_patrol_1",
            Affiliation::Blue,
            "patrol_boat",
            6_000.0,
        ));
        snap.platforms
            .push(platform("blue_patrol_1", Affiliation::Blue, "patrol_boat"));
        let policy = policy();
        let resolutions = resolver.resolve(LabelResolveContext {
            snapshot: &snap,
            labels: &["敌方巡逻艇".into()],
            control_policy: &policy,
        });

        assert_eq!(
            resolutions[0].selected_track_id.as_deref(),
            Some("blue_patrol_1")
        );
    }

    #[test]
    fn score_candidate_protects_configured_own_side_not_hardcoded_blue() {
        let policy = policy();
        let blue_candidate = CandidateTrack {
            track_id: "blue_patrol_1".into(),
            track_id_prefix: Some("blue_patrol_1".into()),
            source_platform_name: Some("blue_patrol_1".into()),
            platform_type: Some("patrol_boat".into()),
            track_classification: "patrol_boat".into(),
            affiliation: Affiliation::Blue,
            iff: "friend".into(),
            weapon_reachable: false,
        };
        let red_candidate = CandidateTrack {
            track_id: "red_patrol_1".into(),
            track_id_prefix: Some("red_patrol_1".into()),
            source_platform_name: Some("red_patrol_1".into()),
            platform_type: Some("patrol_boat".into()),
            track_classification: "patrol_boat".into(),
            affiliation: Affiliation::Red,
            iff: "unknown".into(),
            weapon_reachable: false,
        };

        let blue = score_candidate("蓝方巡逻艇", &blue_candidate, &policy);
        let red = score_candidate("红方巡逻艇", &red_candidate, &policy);

        assert!(
            blue.score >= 35.0,
            "blue should not be protected merely because it is blue: {:?}",
            blue.reasons
        );
        assert!(
            red.score < 35.0,
            "configured own red side should be protected: {:?}",
            red.reasons
        );
    }

    #[test]
    fn label_resolution_registry_deduplicates_pending_by_intent_and_labels() {
        let registry = LabelResolutionRegistry::new();
        let intent = intent(vec!["蓝方指挥所"], vec![]);
        let resolution = LabelResolution {
            label: "蓝方指挥所".into(),
            candidates: vec![],
            selected_track_id: Some("blue_command_post:1".into()),
        };

        let first = registry.submit(&intent, vec![resolution.clone()], 1.0);
        let second = registry.submit(&intent, vec![resolution], 2.0);

        assert_eq!(first.id, second.id);
        assert_eq!(registry.list_pending().len(), 1);
        assert!(registry.has_pending_for_intent("intent-1"));
    }

    fn mission_with(n: usize) -> MissionConfig {
        use openfang_types::umaa::{CommPlan, PlatformLimits, RulesOfEngagement};
        MissionConfig {
            mission_id: "m".into(),
            roe: RulesOfEngagement::default(),
            geofences: Vec::new(),
            platform_limits: PlatformLimits::default(),
            comm_plan: CommPlan::default(),
            contingency_plans: Vec::new(),
            activated_at: None,
            autonomy_mode: Default::default(),
            phase: None,
            objectives: Vec::new(),
            allocations: (0..n)
                .map(|i| TargetAllocation {
                    platform_id: "self".into(),
                    weapon_id: format!("w{i}"),
                    track_id: format!("self:{i}"),
                    allocated_at: 0.0,
                    ..Default::default()
                })
                .collect(),
            target_track_id: None,
            play_name: None,
        }
    }

    #[test]
    fn plan_fingerprint_changes_when_play_or_target_context_changes() {
        let mut mission = mission_with(0);
        let baseline = plan_fingerprint(&mission);

        mission.play_name = Some("Picket".into());
        let with_play = plan_fingerprint(&mission);
        assert_ne!(baseline, with_play, "selected play is approval-visible");

        mission.target_track_id = Some("trk-1".into());
        let with_target = plan_fingerprint(&mission);
        assert_ne!(with_play, with_target, "target context is approval-visible");
    }

    #[test]
    fn authorization_proposals_only_resolve_kept_in_range_indices() {
        let baseline = mission_with(3);
        // Keep 0 and 2; authorize 0 (kept), 1 (not kept), 9 (out of range).
        let refinement = PlanRefinement {
            selected_indices: vec![0, 2],
            authorize_indices: vec![0, 1, 9],
            ..Default::default()
        };
        let proposals = authorization_proposals(&baseline, &refinement);
        assert_eq!(proposals.len(), 1, "only kept+in-range index 0 authorizes");
        assert_eq!(proposals[0].platform_id, "self");
        assert_eq!(proposals[0].track_id, "self:0");
    }

    #[test]
    fn authorization_proposals_default_keep_all_when_no_narrowing() {
        let baseline = mission_with(2);
        let refinement = PlanRefinement {
            selected_indices: vec![], // empty = keep all
            authorize_indices: vec![1],
            ..Default::default()
        };
        let proposals = authorization_proposals(&baseline, &refinement);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].track_id, "self:1");
    }

    #[test]
    fn apply_refinement_bakes_clamped_salvo() {
        let baseline = mission_with(2);
        let refinement = PlanRefinement {
            salvo_overrides: vec![
                SalvoOverride {
                    index: 0,
                    salvo_size: 99, // clamps to MAX_SALVO_SIZE
                },
                SalvoOverride {
                    index: 1,
                    salvo_size: 1, // single
                },
            ],
            ..Default::default()
        };
        let refined = apply_refinement(baseline, &refinement);
        assert_eq!(refined.allocations[0].salvo_size, Some(MAX_SALVO_SIZE));
        assert_eq!(
            refined.allocations[0].weapon_policy.as_deref(),
            Some("salvo")
        );
        assert_eq!(refined.allocations[1].salvo_size, Some(1));
        assert_eq!(
            refined.allocations[1].weapon_policy.as_deref(),
            Some("single")
        );
    }

    #[test]
    fn weapon_employment_policy_forces_single_by_weapon_id() {
        let baseline = mission_with(1);
        let refinement = PlanRefinement {
            salvo_overrides: vec![SalvoOverride {
                index: 0,
                salvo_size: 4,
            }],
            ..Default::default()
        };
        let policy = PlatformControlPolicy {
            weapon_employment: std::collections::HashMap::from([(
                "w0".into(),
                WeaponEmploymentRule {
                    mode: WeaponEmploymentMode::Single,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let refined = apply_refinement_with_policy(baseline, &refinement, &policy, None);
        assert_eq!(refined.allocations[0].salvo_size, Some(1));
        assert_eq!(
            refined.allocations[0].weapon_policy.as_deref(),
            Some("single")
        );
    }

    #[test]
    fn weapon_employment_policy_matches_arksim_weapon_type() {
        let baseline = mission_with(1);
        let refinement = PlanRefinement::default();
        let mut snap = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![PlatformState::minimal("self")],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        snap.platforms[0].onboard_weapons = vec![WeaponState {
            weapon_id: "w0".into(),
            weapon_type: "RED_LOITER_MUN".into(),
            quantity_remaining: 8.0,
            max_range_m: None,
            min_range_m: None,
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        }];
        let policy = PlatformControlPolicy {
            weapon_employment: std::collections::HashMap::from([(
                "red_loiter_mun".into(),
                WeaponEmploymentRule {
                    mode: WeaponEmploymentMode::Salvo,
                    salvo_size: Some(3),
                    max_salvo_size: Some(4),
                },
            )]),
            ..Default::default()
        };

        let refined = apply_refinement_with_policy(baseline, &refinement, &policy, Some(&snap));
        assert_eq!(refined.allocations[0].salvo_size, Some(3));
        assert_eq!(
            refined.allocations[0].weapon_policy.as_deref(),
            Some("salvo")
        );
    }
}
