//! PlatformControlLoop — the live real-time control loop.
//!
//! This is the runtime counterpart to [`crate::tactical_pipeline::TacticalPipeline`]:
//! where the pipeline is the *arbitration core*, this type drives it from real
//! sensor input on a fixed cadence, turning the test-only pipeline into a
//! live-wired control loop:
//!
//! ```text
//! registry.poll_all() ─► DCC.evaluate_intents() ─► Cerebellum (bounded queue + SPGS)
//!                                                        │
//!                                  slow-loop intents ────┤
//!                                                        ▼
//!                                            TacticalPipeline.tick()
//!                                       (compose → gate → engage → dispatch)
//! ```
//!
//! Iron Laws are preserved: the DCC and the slow loop only ever produce
//! [`CandidateIntent`]s; the gate inside the pipeline remains the sole arbiter
//! of what reaches an adapter, and weapon intents become asynchronous
//! engagements rather than blocking the tick.

use std::sync::{Arc, RwLock};

use openfang_platform::AdapterRegistry;
use openfang_types::config::{AutonomyModeProfile, InterventionConfig, PlatformConfig};
use openfang_types::platform::{
    CcaRole, PlatformCapabilities, PlatformCommand, TurnDirection, WorldSnapshot,
};
use openfang_types::tactical::{
    CandidateIntent, CommandPriority, IntentSource, ManualClock, TimeSource,
};

use openfang_runtime::audit::AuditLog;
use openfang_runtime::cca_role::CcaRoleController;
use openfang_runtime::cerebellum::Cerebellum;
use openfang_runtime::cerebellum_services::{CerebellumService, ServiceContext};
use openfang_runtime::cms_service::CommunicationsManagementService;
use openfang_runtime::command_gate::ConfigurableApproval;
use openfang_runtime::direct_channel::DirectCommandChannel;
use openfang_runtime::ewms_service::ElectronicWarfareManagementService;
use openfang_runtime::fleet_manager::FleetManager;
use openfang_runtime::geo_zones::NavIntent;
use openfang_runtime::intent_extractor::{FlankSide, StructuredIntent};
use openfang_runtime::intervention::InterventionGate;
use openfang_runtime::maneuver_service::ManeuverManagementService;
use openfang_runtime::mission_approval::MissionApprovalRegistry;
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::sensor_fusion::{FusedTrack, FusionOutput, SensorFusion};
use openfang_runtime::sensor_management::SensorManagementService;
use openfang_runtime::survivability_service::SurvivabilityService;
use openfang_runtime::target_authorization::TargetAuthorizationRegistry;
use openfang_runtime::track_manager::{SensorContact, TrackManager};

use crate::tactical_pipeline::{TacticalPipeline, TickReport};

/// Outcome of a single control-loop step.
#[derive(Debug, Default, Clone)]
pub struct StepReport {
    /// Whether a fresh snapshot was polled this step.
    pub polled: bool,
    /// DCC intents emitted (critical + high) this step.
    pub dcc_intents: usize,
    /// PSS (Platform Survivability Service) reflex intents this step.
    pub pss_intents: usize,
    /// SMS (Sensor Management Service) reflex intents this step.
    pub sms_intents: usize,
    /// MMS (Maneuver Management Service) route/CPA intents this step.
    pub mms_intents: usize,
    /// EWMS (Electronic Warfare Management Service) jam/EW intents this step.
    pub ewms_intents: usize,
    /// CMS (Communications Management Service) link-strategy intents this step.
    pub cms_intents: usize,
    /// Intents that survived the cerebellum SPGS pre-screen this step.
    pub survivors: usize,
    /// Number of fused tracks produced from the latest snapshot.
    pub fused_tracks: usize,
    /// Number of raw contacts fed into the UMAA track manager this step.
    pub track_correlations: usize,
    /// The pipeline (gate/dispatch/engagement) report for this step.
    pub pipeline: TickReport,
    /// Whether link-driven degradation is forcing the degraded profile (M4-U6).
    pub link_degraded: bool,
    /// Dangerous queued intents dropped this step because they aged past the
    /// staleness window (M4-U6) — a stale fire order never replays.
    pub stale_dropped: usize,
}

/// The live platform control loop.
pub struct PlatformControlLoop {
    registry: Arc<AdapterRegistry>,
    pipeline: TacticalPipeline,
    dcc: DirectCommandChannel,
    cerebellum: Cerebellum,
    role_controller: CcaRoleController,
    fleet_manager: FleetManager,
    clock: ManualClock,
    own_platform_id: String,
    last_effective_role: Option<CcaRole>,
    latest: Option<WorldSnapshot>,
    latest_fusion: Option<FusionOutput>,
    sensor_fusion: SensorFusion,
    track_manager: TrackManager,
    /// PSS (Platform Survivability Service) — emits Critical RTB / slow
    /// reflexes when own-platform fuel or damage breach thresholds. Sits
    /// between SMS poll and the role/fleet logic in the step pipeline.
    survivability: SurvivabilityService,
    /// SMS (Sensor Management Service) — owns sensor posture enforcement and
    /// operator sensor override arbitration.
    sensor_management: SensorManagementService,
    /// MMS (Maneuver Management Service) — deterministic route planning,
    /// semantic zone navigation, and CPA avoidance reflexes.
    maneuver: ManeuverManagementService,
    /// EWMS (Electronic Warfare Management Service) — fusion-driven defensive
    /// jam cues over the platform's jammers.
    ewms: ElectronicWarfareManagementService,
    /// CMS (Communications Management Service) — link-strategy management as an
    /// explicit hot-path closed loop.
    cms: CommunicationsManagementService,
    target_registry: Arc<TargetAuthorizationRegistry>,
    mission_approvals: Arc<MissionApprovalRegistry>,
    intervention_gate: Option<Arc<RwLock<InterventionGate>>>,
    /// **Effective** autonomy-mode profile shared with the gate. Hot-swappable
    /// so operator-driven profile switches (config reload, API mutation) take
    /// effect on the very next tick without rebuilding the pipeline. Under a
    /// degraded link this holds [`Self::degraded_profile`]; otherwise it holds
    /// [`Self::operator_profile`]. Recomputed at the top of every [`Self::step`].
    autonomy_profile: Option<Arc<RwLock<AutonomyModeProfile>>>,
    /// Operator-intended profile (M4-U6). The single source of truth for "what
    /// the controller asked for", kept separate from the gate-side *effective*
    /// profile so link-driven degradation can be undone losslessly on recovery.
    operator_profile: AutonomyModeProfile,
    /// Profile to fall back to when the link degrades (`Poor`/`Lost`). Resolved
    /// once from `[platform.autonomy.degraded_profile]`. `None` disables
    /// link-driven degradation (the operator profile always stands).
    degraded_profile: Option<AutonomyModeProfile>,
    /// Operator/API override of the observed link quality (M4-U6). `None` ⇒ use
    /// the link quality observed on the own-platform snapshot; `Some(q)` ⇒ the
    /// simulated value wins (used by the live-integration degradation matrix
    /// without physically tearing down the OFP link). Shared with the kernel.
    link_quality_override: Arc<RwLock<Option<openfang_types::platform::LinkQuality>>>,
    /// Staleness window (s) for queued dangerous commands (M4-U6). A
    /// `FireAtTarget`/`AssignMission` older than this is dropped before the
    /// pipeline so a fire order from before a blackout is never replayed.
    stale_command_window_s: f64,
    /// Shared safety state — kept so the slow loop can read the live ROE level
    /// (weapon-release authority) when deciding whether to honor LLM fire
    /// authorizations.
    restrictions: Arc<OpRestrictionsManager>,
}

impl PlatformControlLoop {
    /// Build a control loop around an existing (typically already-built)
    /// registry plus the shared safety state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<AdapterRegistry>,
        restrictions: Arc<OpRestrictionsManager>,
        audit: Arc<AuditLog>,
        capabilities: PlatformCapabilities,
        own_platform_id: impl Into<String>,
        tick_hz: f64,
        queue_cap: usize,
        weapon_quorum: u32,
        approval_window_s: f64,
    ) -> Self {
        let own_platform_id = own_platform_id.into();
        let target_registry = Arc::new(TargetAuthorizationRegistry::new());
        let mission_approvals = Arc::new(MissionApprovalRegistry::new());
        let pipeline = TacticalPipeline::new(
            registry.clone(),
            restrictions.clone(),
            audit,
            capabilities,
            weapon_quorum,
            approval_window_s,
        );
        let cerebellum = Cerebellum::new(tick_hz, queue_cap, restrictions.clone());
        Self {
            registry,
            pipeline,
            dcc: DirectCommandChannel::new(),
            cerebellum,
            role_controller: CcaRoleController::new(own_platform_id.clone(), CcaRole::Adaptive),
            fleet_manager: FleetManager::new(own_platform_id.clone()),
            clock: ManualClock::new(0.0),
            own_platform_id,
            last_effective_role: None,
            latest: None,
            latest_fusion: None,
            sensor_fusion: SensorFusion::new(),
            track_manager: TrackManager::new(),
            survivability: SurvivabilityService::default(),
            sensor_management: SensorManagementService::default(),
            maneuver: ManeuverManagementService::new(
                openfang_types::config::ManeuverConfig::default(),
                openfang_runtime::geo_zones::GeoZoneRegistry::default(),
                restrictions.geofences(),
            ),
            ewms: ElectronicWarfareManagementService::new(),
            cms: CommunicationsManagementService::new(),
            target_registry,
            mission_approvals,
            intervention_gate: None,
            autonomy_profile: None,
            operator_profile: AutonomyModeProfile::default(),
            degraded_profile: None,
            link_quality_override: Arc::new(RwLock::new(None)),
            stale_command_window_s:
                openfang_types::config::FederationConfig::DEFAULT_STALE_COMMAND_WINDOW_S,
            restrictions,
        }
    }

    /// Convenience constructor driven by [`PlatformConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn from_config(
        registry: Arc<AdapterRegistry>,
        cfg: &PlatformConfig,
        restrictions: Arc<OpRestrictionsManager>,
        audit: Arc<AuditLog>,
        capabilities: PlatformCapabilities,
        target_registry: Arc<TargetAuthorizationRegistry>,
        mission_approvals: Arc<MissionApprovalRegistry>,
    ) -> Self {
        let own_platform_id = cfg.own_platform_id.clone();
        let intervention_gate = Arc::new(RwLock::new(InterventionGate::new(
            cfg.intervention.clone(),
            Arc::clone(&target_registry),
            Arc::clone(&mission_approvals),
        )));
        let approval = Arc::new(ConfigurableApproval::with_shared_gate(Arc::clone(
            &intervention_gate,
        )));
        // Active autonomy profile (gate-side hard envelope). Held behind an
        // Arc<RwLock<_>> so config hot-reload can hot-swap it without
        // rebuilding the pipeline. The operator profile is the same at boot;
        // link-driven degradation may later make the gate-side *effective*
        // profile diverge (M4-U6).
        let operator_profile = cfg.autonomy.active();
        let degraded_profile = cfg
            .autonomy
            .degraded_profile
            .as_ref()
            .filter(|id| !id.is_empty())
            .and_then(|id| cfg.autonomy.profile(id).cloned());
        let autonomy_profile = Arc::new(RwLock::new(operator_profile.clone()));
        let mut pipeline = TacticalPipeline::new_with_autonomy(
            registry.clone(),
            restrictions.clone(),
            audit,
            capabilities,
            cfg.weapon_quorum,
            cfg.approval_window_s,
            approval,
            Arc::clone(&autonomy_profile),
        );
        pipeline.set_engagement_cooldown_secs(cfg.engagement_cooldown_secs);
        pipeline.set_weapon_cooldowns_secs(cfg.weapon_cooldowns_secs.clone());
        let cerebellum = Cerebellum::new(cfg.tick_hz, 512, restrictions.clone());
        // DCC reflexes are now config-driven (Phase 1): the master switch, the
        // built-in rule install, and the evasion/response parameters all come
        // from `cfg.dcc` instead of being hard-coded at boot.
        let mut dcc = DirectCommandChannel::new();
        if cfg.dcc.use_default_rules {
            dcc.load_rules(openfang_runtime::direct_channel::tactical_rules(
                &cfg.dcc.evasion,
            ));
        }
        if !cfg.dcc.enabled {
            dcc.set_enabled(false);
        }
        Self {
            registry,
            pipeline,
            dcc,
            cerebellum,
            role_controller: CcaRoleController::new(own_platform_id.clone(), CcaRole::Adaptive),
            fleet_manager: FleetManager::new(own_platform_id.clone()),
            clock: ManualClock::new(0.0),
            own_platform_id,
            last_effective_role: None,
            latest: None,
            latest_fusion: None,
            sensor_fusion: SensorFusion::new(),
            track_manager: TrackManager::new(),
            survivability: SurvivabilityService::default(),
            sensor_management: SensorManagementService::from_config(&cfg.sensor_policy),
            maneuver: ManeuverManagementService::new(
                cfg.maneuver.clone(),
                openfang_runtime::geo_zones::GeoZoneRegistry::from_config(&cfg.geo_zones),
                restrictions.geofences(),
            ),
            ewms: ElectronicWarfareManagementService::new(),
            cms: CommunicationsManagementService::new(),
            target_registry,
            mission_approvals,
            intervention_gate: Some(intervention_gate),
            autonomy_profile: Some(autonomy_profile),
            operator_profile,
            degraded_profile,
            link_quality_override: Arc::new(RwLock::new(None)),
            stale_command_window_s: cfg.federation.effective_stale_window_s(),
            restrictions,
        }
    }

    /// Read-only handle to the active autonomy-mode profile (gate-side).
    /// Returns `None` for control loops built via [`PlatformControlLoop::new`]
    /// (which uses a fixed, non-hot-swappable open profile).
    pub fn autonomy_profile_handle(&self) -> Option<Arc<RwLock<AutonomyModeProfile>>> {
        self.autonomy_profile.clone()
    }

    /// Hot-swap the **operator-intended** autonomy-mode profile. Takes effect on
    /// the next pipeline tick. Returns the previous operator profile id for
    /// audit purposes; returns `None` if this control loop was built without an
    /// autonomy profile (i.e. via [`PlatformControlLoop::new`]).
    ///
    /// The gate-side *effective* profile is re-resolved immediately: under a
    /// healthy link it equals the new operator profile; under a degraded link
    /// the [`Self::degraded_profile`] continues to stand until the link
    /// recovers (M4-U6). Recovery is lossless because the operator intent is
    /// stored here, never clobbered by degradation.
    pub fn set_autonomy_profile(&mut self, profile: AutonomyModeProfile) -> Option<String> {
        self.autonomy_profile.as_ref()?;
        let previous = self.operator_profile.id.clone();
        self.operator_profile = profile;
        self.apply_effective_profile();
        Some(previous)
    }

    /// Shared handle to the operator/API link-quality override (M4-U6). The
    /// kernel grabs this once at boot so `set_simulated_link_quality` can inject
    /// a simulated link bucket that the control loop honours on the next tick.
    pub fn link_quality_override_handle(
        &self,
    ) -> Arc<RwLock<Option<openfang_types::platform::LinkQuality>>> {
        Arc::clone(&self.link_quality_override)
    }

    /// Link quality observed on the latest own-platform snapshot, if any.
    /// `None` until the first poll carries a [`LinkStatusReport`].
    pub fn observed_link_quality(&self) -> Option<openfang_types::platform::LinkQuality> {
        self.latest
            .as_ref()
            .and_then(|snap| snap.platforms.iter().find(|p| p.id == self.own_platform_id))
            .and_then(|p| p.link)
            .map(|link| link.quality)
    }

    /// Effective link quality the degradation trigger evaluates: the operator
    /// override wins; otherwise the observed bucket; otherwise `Excellent`.
    pub fn effective_link_quality(&self) -> openfang_types::platform::LinkQuality {
        if let Some(q) = self.link_quality_override.read().ok().and_then(|g| *g) {
            return q;
        }
        self.observed_link_quality()
            .unwrap_or(openfang_types::platform::LinkQuality::Excellent)
    }

    /// Resolve the gate-side *effective* profile from the operator profile, the
    /// effective link quality, and the configured degraded profile. Returns
    /// `(profile, degraded)`.
    fn resolve_effective_profile(&self) -> (AutonomyModeProfile, bool) {
        if self.effective_link_quality().should_force_defensive() {
            if let Some(degraded) = &self.degraded_profile {
                return (degraded.clone(), true);
            }
        }
        (self.operator_profile.clone(), false)
    }

    /// Recompute and publish the gate-side effective profile. Returns whether
    /// degradation is currently active. Cheap; safe to call every tick.
    fn apply_effective_profile(&self) -> bool {
        let (effective, degraded) = self.resolve_effective_profile();
        if let Some(handle) = &self.autonomy_profile {
            let changed = handle
                .read()
                .ok()
                .map(|cur| cur.id != effective.id)
                .unwrap_or(true);
            if changed {
                if let Ok(mut guard) = handle.write() {
                    *guard = effective;
                }
            }
        }
        degraded
    }

    /// Install the standard tactical DCC ruleset (chaff/collision/RTB/…).
    pub fn install_default_dcc_rules(&mut self) {
        for rule in openfang_runtime::direct_channel::default_tactical_rules() {
            self.dcc.add_rule(rule);
        }
    }

    /// Mutable access to the DCC for custom rule installation.
    pub fn dcc_mut(&mut self) -> &mut DirectCommandChannel {
        &mut self.dcc
    }

    /// Replace the standing slow-loop plan (authoritative; held by the gate path).
    pub fn set_active_plan(&mut self, intents: Vec<CandidateIntent>) {
        self.pipeline.set_active_plan(intents);
    }

    /// Live weapon-release authority (ROE) the loop is operating under. Read by
    /// the slow loop to decide whether an LLM fire authorization may be honored
    /// (auto-authorize is allowed only under `WeaponsFree`).
    pub fn weapon_release_level(&self) -> openfang_types::umaa::WeaponReleaseLevel {
        self.restrictions.get_roe().weapon_release_authority
    }

    /// Enable/disable a DCC reflex rule by name (contingency-plan actuation from
    /// the slow loop). Returns `true` if such a rule existed.
    pub fn set_dcc_rule_enabled(&mut self, rule_name: &str, enabled: bool) -> bool {
        self.dcc.set_rule_enabled(rule_name, enabled)
    }

    /// Brain → own platform: assign the tactical role this node should adopt
    /// (own-scope workflow decision). The role drives the cerebellum lane posture
    /// on the next step. In standalone mode this persists; in member mode a
    /// lead's fleet-assigned mission role takes precedence when present.
    pub fn set_own_role(&mut self, role: openfang_types::platform::CcaRole) {
        self.role_controller.assign(role);
    }

    /// Lead → members: allocate and distribute member roles for a fired
    /// formation-scope workflow. Capability-gated allocation (an unarmed LSUAV is
    /// never given a jammer/strike role); each assignment is dispatched as an
    /// `AssignMission` carrying the role, which a member instance consumes via the
    /// same brain→cerebellum contract. Returns the assignments for audit.
    pub fn assign_formation_roles(
        &mut self,
        workflow: &str,
        now: f64,
    ) -> Vec<openfang_runtime::fleet_manager::RoleAssignment> {
        let assignments = self.fleet_manager.allocate_formation_roles(workflow);
        let mission_type = openfang_runtime::fleet_manager::workflow_mission_type(workflow);
        for a in &assignments {
            let params_json = serde_json::json!({ "role": a.role }).to_string();
            self.cerebellum.submit(CandidateIntent::new(
                openfang_types::platform::PlatformCommand::AssignMission {
                    uav_id: a.member_id.clone(),
                    mission_type: mission_type.clone(),
                    params_json,
                },
                CommandPriority::Normal,
                IntentSource::Llm {
                    agent_id: "fma".into(),
                },
                now,
                a.reason.clone(),
            ));
        }
        assignments
    }

    /// Submit a transient intent (LLM/operator) into the fast-loop queue.
    pub fn submit_intent(&mut self, intent: CandidateIntent) {
        match &intent.command {
            PlatformCommand::SensorOn { sensor_id, .. } => {
                self.sensor_management.note_operator_sensor_intent(
                    sensor_id,
                    "on",
                    self.clock.now_secs(),
                );
            }
            PlatformCommand::SensorOff { sensor_id, .. } => {
                self.sensor_management.note_operator_sensor_intent(
                    sensor_id,
                    "off",
                    self.clock.now_secs(),
                );
            }
            PlatformCommand::SensorSetMode {
                sensor_id, mode, ..
            } => {
                self.sensor_management.note_operator_sensor_intent(
                    sensor_id,
                    mode,
                    self.clock.now_secs(),
                );
            }
            _ => {}
        }
        self.cerebellum.submit(intent);
    }

    /// Current control-loop time in the same domain used by federation staleness
    /// checks. This tracks the latest polled simulator snapshot timestamp.
    pub fn now_secs(&self) -> f64 {
        self.clock.now_secs()
    }

    /// Connect every configured adapter.
    pub async fn connect(&self) -> Result<(), openfang_platform::PlatformError> {
        self.registry.connect_all().await
    }

    /// Latest polled snapshot, if any.
    pub fn latest_snapshot(&self) -> Option<&WorldSnapshot> {
        self.latest.as_ref()
    }

    /// Latest fused-track view, if a snapshot has been polled.
    pub fn latest_fusion(&self) -> Option<&FusionOutput> {
        self.latest_fusion.as_ref()
    }

    /// Latest SMS status view, joined with current sensor telemetry.
    pub fn sensor_management_statuses(
        &self,
    ) -> Vec<openfang_runtime::sensor_management::SensorManagementStatus> {
        let own = self
            .latest
            .as_ref()
            .and_then(|snapshot| snapshot.find_platform(&self.own_platform_id));
        self.sensor_management.status_for(own)
    }

    /// Latest fleet picture pulled off the world snapshot, cloned for the
    /// kernel to publish to the federation engine / dashboard. Returns
    /// `None` until the first poll succeeds or when the underlying snapshot
    /// carries no fleet view.
    pub fn latest_fleet_snapshot(&self) -> Option<openfang_types::platform::FleetSnapshot> {
        self.latest.as_ref().and_then(|snap| snap.fleet.clone())
    }

    /// Latest MMS route plan, if any.
    pub fn latest_route_plan(&self) -> Option<&openfang_types::route::RoutePlan> {
        self.maneuver.active_plan()
    }

    /// Set MMS navigation intent from parsed objective text.
    pub fn set_mms_objective(&mut self, text: &str) -> bool {
        self.maneuver.set_objective_text(text)
    }

    /// Set MMS navigation intent directly.
    pub fn set_mms_nav_intent(&mut self, intent: Option<openfang_runtime::geo_zones::NavIntent>) {
        self.maneuver.set_nav_intent(intent);
    }

    /// Brain → MMS bridge: mirror structured tactical maneuver semantics into
    /// the deterministic Maneuver Management Service. This keeps the slow-loop
    /// planner and operator input on the same small-brain route/motion lane.
    pub fn sync_mms_from_structured_intent(&mut self, intent: &StructuredIntent) -> bool {
        let target = intent
            .target_track_ids
            .first()
            .cloned()
            .or_else(|| intent.target_labels.first().cloned());
        let standoff_m = intent.standoff_m.unwrap_or(3000.0);

        if intent.maneuver.flank_approach {
            if let Some(track_label) = target.clone() {
                self.maneuver.set_nav_intent(Some(NavIntent::FlankStandoff {
                    track_label,
                    range_m: standoff_m,
                    turn_direction: intent
                        .flank_side
                        .map(turn_from_flank)
                        .or_else(|| intent.maneuver.turn.map(turn_from_flank)),
                }));
                return true;
            }
        }

        if let (Some(track_label), Some(range_m)) = (target, intent.standoff_m) {
            self.maneuver.set_nav_intent(Some(NavIntent::Standoff {
                track_label,
                range_m,
            }));
            return true;
        }

        if intent.maneuver.heading_deg.is_some()
            || intent.maneuver.heading_delta_deg.is_some()
            || intent.maneuver.turn.is_some()
            || intent.maneuver.speed_ms.is_some()
        {
            self.maneuver
                .set_nav_intent(Some(NavIntent::DirectManeuver {
                    heading_deg: intent.maneuver.heading_deg,
                    heading_delta_deg: intent.maneuver.heading_delta_deg,
                    turn_direction: intent.maneuver.turn.map(turn_from_flank),
                    speed_ms: intent.maneuver.speed_ms,
                }));
            return true;
        }

        self.set_mms_objective(&intent.raw_text)
    }

    /// The shared pipeline (for signing / launching weapon engagements).
    pub fn pipeline_mut(&mut self) -> &mut TacticalPipeline {
        &mut self.pipeline
    }

    pub fn target_registry(&self) -> Arc<TargetAuthorizationRegistry> {
        Arc::clone(&self.target_registry)
    }

    /// Shared mission-plan approval registry (persistent `Confirm`/`Quorum`
    /// backing store for the slow-loop `mission_approval` checkpoint).
    pub fn mission_approvals(&self) -> Arc<MissionApprovalRegistry> {
        Arc::clone(&self.mission_approvals)
    }

    pub fn authorize_target(
        &mut self,
        platform_id: &str,
        track_id: &str,
        operator_id: &str,
        authorized_at: f64,
    ) {
        self.target_registry
            .authorize(platform_id, track_id, operator_id, authorized_at);
        // A deliberate (re-)authorization is the operator's signal to (re-)strike
        // this target. Clear any fire-once lock so the standing plan's next fire
        // against this engagement is allowed exactly once more.
        self.pipeline.release_engagement(platform_id, track_id);
    }

    /// Shared intervention gate (fast-loop approval engine). The slow cognitive
    /// loop reuses this same handle so a single hot-reload updates both the
    /// `mission_approval` checkpoint and the fast-loop weapon gate.
    pub fn intervention_gate(&self) -> Option<Arc<RwLock<InterventionGate>>> {
        self.intervention_gate.clone()
    }

    pub fn update_intervention_config(&self, config: InterventionConfig) -> bool {
        let Some(gate) = &self.intervention_gate else {
            return false;
        };
        *gate.write().unwrap_or_else(|e| e.into_inner()) = InterventionGate::new(
            config,
            Arc::clone(&self.target_registry),
            Arc::clone(&self.mission_approvals),
        );
        true
    }

    pub fn update_cooldown_config(
        &mut self,
        engagement_cooldown_secs: f64,
        weapon_cooldowns_secs: std::collections::HashMap<String, f64>,
    ) {
        self.pipeline
            .set_engagement_cooldown_secs(engagement_cooldown_secs);
        self.pipeline
            .set_weapon_cooldowns_secs(weapon_cooldowns_secs);
    }

    /// Run one full control step: poll → DCC → cerebellum → pipeline.
    pub async fn step(&mut self) -> StepReport {
        let mut report = StepReport::default();

        // ── Stage 1: SMS (Sensor Management Service) ──────────────────────
        // Pull the freshest world state and update sensor fusion + track mgr.
        // No allocation on the hot path beyond the snapshot itself.
        match self.registry.poll_all().await {
            Ok(snap) => {
                self.clock.set(snap.timestamp);
                let fusion = self.sensor_fusion.update(&snap);
                report.fused_tracks = fusion.fused_tracks.len();
                // Unify the dual-track systems: the UMAA TrackManager correlates
                // against the *fused* picture (stable ids + Kalman-smoothed
                // positions) rather than re-deriving its own association from raw
                // per-sensor returns. Both track stores now share one id space.
                report.track_correlations = correlate_tracks(&mut self.track_manager, &fusion);
                self.latest_fusion = Some(fusion);
                self.latest = Some(snap);
                report.polled = true;
            }
            Err(e) => {
                tracing::debug!("control loop poll skipped: {e}");
            }
        }
        let snapshot = self.latest.clone();
        // Unified fused-track picture for this tick, shared read-only with every
        // cerebellum service via `ServiceContext::fused_tracks`. Cloned once (the
        // snapshot is already cloned above) so services borrow an owned local and
        // never contend with the `&mut self.<service>` evaluate calls below.
        let fused_tracks: Vec<FusedTrack> = self
            .latest_fusion
            .as_ref()
            .map(|fusion| fusion.fused_tracks.clone())
            .unwrap_or_default();

        // ── Stage 1b: CMS link-driven autonomy degradation (M4-U6) ────────
        // Recompute the gate-side effective profile from (operator profile,
        // effective link quality, degraded profile) every tick — before any
        // stage reads the autonomy envelope. A degraded link forces the
        // degraded profile; recovery restores the operator profile losslessly.
        report.link_degraded = self.apply_effective_profile();

        // ── Stage 2: PSS (Platform Survivability Service) ─────────────────
        // Watch own-platform fuel/damage every tick and emit Critical RTB /
        // slow-and-RTB reflexes. Runs before role/fleet so survival intents
        // are present when the cerebellum drains the queue.
        if let Some(snap) = snapshot.as_ref() {
            if let Some(self_state) = snap.platforms.iter().find(|p| p.id == self.own_platform_id) {
                let caps = self.registry.combined_capabilities();
                let posture = self.role_controller.current();
                let autonomy_guard = self.autonomy_profile.as_ref().and_then(|p| p.read().ok());
                let ctx = ServiceContext {
                    snapshot: Some(snap),
                    own_platform: Some(self_state),
                    fused_tracks: &fused_tracks,
                    autonomy: autonomy_guard.as_deref(),
                    capabilities: &caps,
                    posture,
                    now: self.clock.now_secs(),
                    own_platform_id: &self.own_platform_id,
                };
                let pss_out = self.survivability.evaluate(&ctx);
                report.pss_intents = pss_out.intents.len();
                for intent in pss_out.intents {
                    self.cerebellum.submit(intent);
                }
            }
        }

        // ── Stage 2a: SMS (Sensor Management Service) ────────────────────
        // Owns sensor posture enforcement so role posture never overwrites an
        // explicit operator sensor command.
        if let Some(snap) = snapshot.as_ref() {
            if let Some(self_state) = snap.platforms.iter().find(|p| p.id == self.own_platform_id) {
                let caps = self.registry.combined_capabilities();
                let posture = self.role_controller.current();
                let autonomy_guard = self.autonomy_profile.as_ref().and_then(|p| p.read().ok());
                // Refresh the live ROE so the sensor policy engine arbitrates the
                // active-emitter release matrix against the current authority.
                let roe = self.restrictions.get_roe().weapon_release_authority;
                self.sensor_management.set_roe(roe);
                let ctx = ServiceContext {
                    snapshot: Some(snap),
                    own_platform: Some(self_state),
                    fused_tracks: &fused_tracks,
                    autonomy: autonomy_guard.as_deref(),
                    capabilities: &caps,
                    posture,
                    now: self.clock.now_secs(),
                    own_platform_id: &self.own_platform_id,
                };
                let sms_out = self.sensor_management.evaluate(&ctx);
                report.sms_intents = sms_out.intents.len();
                // Persist SMS rationale to the audit trail for post-hoc review.
                if !sms_out.audit_hints.is_empty() {
                    let audit = self.pipeline.audit();
                    for hint in &sms_out.audit_hints {
                        audit.record(
                            "sms",
                            openfang_runtime::audit::AuditAction::ConfigChange,
                            hint.detail.clone().unwrap_or_default(),
                            hint.event.clone(),
                        );
                    }
                }
                for intent in sms_out.intents {
                    self.cerebellum.submit(intent);
                }
            }
        }

        // ── Stage 2b: MMS (Maneuver Management Service) ───────────────────
        // Deterministic route planning + CPA reflexes. Runs parallel to DCC;
        // collision_avoidance in DCC is disabled when MMS CPA is active.
        self.maneuver.set_link_degraded(report.link_degraded);
        if let Some(snap) = snapshot.as_ref() {
            if let Some(self_state) = snap.platforms.iter().find(|p| p.id == self.own_platform_id) {
                let caps = self.registry.combined_capabilities();
                let posture = self.role_controller.current();
                let autonomy_guard = self.autonomy_profile.as_ref().and_then(|p| p.read().ok());
                let ctx = ServiceContext {
                    snapshot: Some(snap),
                    own_platform: Some(self_state),
                    fused_tracks: &fused_tracks,
                    autonomy: autonomy_guard.as_deref(),
                    capabilities: &caps,
                    posture,
                    now: self.clock.now_secs(),
                    own_platform_id: &self.own_platform_id,
                };
                let mms_out = self.maneuver.evaluate(&ctx);
                report.mms_intents = mms_out.intents.len();
                for intent in mms_out.intents {
                    self.cerebellum.submit(intent);
                }
            }
        }

        // ── Stage 2c: CMS (Communications Management Service) ─────────────
        // Link-strategy closed loop: observe link quality, command the matching
        // transmission strategy (idempotent). Distinct from the out-of-loop
        // comm_monitor ping task and the Stage 1b autonomy-degradation gate.
        if let Some(snap) = snapshot.as_ref() {
            if let Some(self_state) = snap.platforms.iter().find(|p| p.id == self.own_platform_id) {
                let caps = self.registry.combined_capabilities();
                let posture = self.role_controller.current();
                let autonomy_guard = self.autonomy_profile.as_ref().and_then(|p| p.read().ok());
                let ctx = ServiceContext {
                    snapshot: Some(snap),
                    own_platform: Some(self_state),
                    fused_tracks: &fused_tracks,
                    autonomy: autonomy_guard.as_deref(),
                    capabilities: &caps,
                    posture,
                    now: self.clock.now_secs(),
                    own_platform_id: &self.own_platform_id,
                };
                let cms_out = self.cms.evaluate(&ctx);
                report.cms_intents = cms_out.intents.len();
                if !cms_out.audit_hints.is_empty() {
                    let audit = self.pipeline.audit();
                    for hint in &cms_out.audit_hints {
                        audit.record(
                            "cms",
                            openfang_runtime::audit::AuditAction::ConfigChange,
                            hint.detail.clone().unwrap_or_default(),
                            hint.event.clone(),
                        );
                    }
                }
                for intent in cms_out.intents {
                    self.cerebellum.submit(intent);
                }
            }
        }

        // ── Stage 3: EWMS / DCC reflexes ────────────────────────────────
        // EWMS (Electronic Warfare Management) is now a first-class cerebellum
        // service driving fusion-confirmed jam cues; the legacy DCC rules
        // (chaff, emergency RTB, etc) still run alongside it. MMS handles route
        // planning + CPA. Both lanes only propose intents — ACS/SPGS arbitrate.
        if let Some(snap) = snapshot.as_ref() {
            if let Some(self_state) = snap.platforms.iter().find(|p| p.id == self.own_platform_id) {
                let caps = self.registry.combined_capabilities();
                let posture = self.role_controller.current();
                let autonomy_guard = self.autonomy_profile.as_ref().and_then(|p| p.read().ok());
                let ctx = ServiceContext {
                    snapshot: Some(snap),
                    own_platform: Some(self_state),
                    fused_tracks: &fused_tracks,
                    autonomy: autonomy_guard.as_deref(),
                    capabilities: &caps,
                    posture,
                    now: self.clock.now_secs(),
                    own_platform_id: &self.own_platform_id,
                };
                let ewms_out = self.ewms.evaluate(&ctx);
                report.ewms_intents = ewms_out.intents.len();
                if !ewms_out.audit_hints.is_empty() {
                    let audit = self.pipeline.audit();
                    for hint in &ewms_out.audit_hints {
                        audit.record(
                            "ewms",
                            openfang_runtime::audit::AuditAction::ConfigChange,
                            hint.detail.clone().unwrap_or_default(),
                            hint.event.clone(),
                        );
                    }
                }
                for intent in ewms_out.intents {
                    self.cerebellum.submit(intent);
                }
            }
        }
        if let Some(snap) = snapshot.as_ref() {
            let now = self.clock.now_secs();
            let dcc_intents = self.dcc.evaluate_intents(snap, &self.own_platform_id, now);
            report.dcc_intents = dcc_intents.len();
            for intent in dcc_intents {
                self.cerebellum.submit(intent);
            }
        }

        // ── Stage 4: Role / fleet management (formation + CCA role) ───────
        // Side-effect-free autonomy layers that only propose intents. The gate
        // remains the single path to actuation; capability gating happens here.
        if let Some(snap) = snapshot.as_ref() {
            let now = self.clock.now_secs();

            if let Some(fleet) = snap.fleet.clone() {
                self.fleet_manager.ingest(fleet);
                for action in self.fleet_manager.evaluate() {
                    self.cerebellum.submit(CandidateIntent::new(
                        action.command,
                        CommandPriority::High,
                        IntentSource::Dcc {
                            rule_name: "fleet_manager".into(),
                        },
                        now,
                        action.reason,
                    ));
                }
            }

            if let Some(self_state) = snap.platforms.iter().find(|p| p.id == self.own_platform_id) {
                if let Some(role) = snap
                    .fleet
                    .as_ref()
                    .and_then(|fleet| fleet.get(&self.own_platform_id))
                    .and_then(|uav| uav.mission.as_ref())
                    .and_then(|mission| mission.role)
                {
                    self.role_controller.assign(role);
                }

                // Gate the cerebellum's domain reflex lanes to this platform's
                // real capabilities (e.g. a no-jammer airframe → EW lane inert).
                self.cerebellum
                    .set_capabilities(self.registry.combined_capabilities());

                let posture = self.role_controller.posture(self_state);
                if self.last_effective_role != Some(posture.role) {
                    self.last_effective_role = Some(posture.role);
                    // Brain → cerebellum: fan the posture out to the lanes, then
                    // emit capability-gated posture-enforcement intents.
                    self.cerebellum.set_posture(posture);
                    self.cerebellum.enforce_posture(self_state, now);
                }
            }
        }

        // ── Stage 5: Cerebellum drain + SPGS pre-screen ──────────────────
        // Bounded queue drain + cheap ROE / limit / geofence rejection. Heavy
        // arbitration happens in the pipeline below.
        let survivors = self.cerebellum.tick(snapshot.as_ref()).intents;

        // ── Stage 5b: Federation staleness filter (M4-U6) ────────────────
        // Drop dangerous queued intents (`FireAtTarget`/`AssignMission`) older
        // than the staleness window before they reach the pipeline, so a fire
        // order issued before a link blackout is never replayed on recovery.
        let now = self.clock.now_secs();
        let (survivors, dropped) = openfang_runtime::federation::filter_stale_by_window(
            survivors,
            self.stale_command_window_s,
            now,
        );
        report.stale_dropped = dropped.len();
        for intent in &dropped {
            tracing::warn!(
                reason = %intent.reason,
                age_s = now - intent.issued_at,
                "federation: dropped stale dangerous intent (not replayed)"
            );
        }
        report.survivors = survivors.len();

        // ── Stage 6: ACS + SPGS + WMS pipeline → AdapterRegistry ─────────
        // Authoritative compose → gate → engage → dispatch. CMS-side link
        // monitoring runs out-of-loop in its own tokio task (see
        // `comm_monitor.rs`) so the hot path stays allocation-light.
        report.pipeline = self
            .pipeline
            .tick(survivors, snapshot.as_ref(), &self.clock)
            .await;

        report
    }
}

/// Feed the unified [`FusionOutput`] into the UMAA [`TrackManager`].
///
/// The fusion engine is the authoritative correlator (stable `track_id`,
/// Kalman-smoothed position, fused quality); the TrackManager layers UMAA
/// quality/identification grading on top of that *same* id space rather than
/// running a second, divergent association pass over raw sensor returns.
fn correlate_tracks(manager: &mut TrackManager, fusion: &FusionOutput) -> usize {
    let mut count = 0;
    for fused in &fusion.fused_tracks {
        let (lat, lon, _alt) = fused.position;
        manager.correlate(SensorContact {
            contact_id: fused.track_id.clone(),
            classification: fused.classification.clone(),
            lat,
            lon,
            speed_ms: fused.speed_ms,
            heading_deg: fused.heading_deg,
            range_m: f64::INFINITY,
            bearing_deg: 0.0,
            quality: fused.quality,
            timestamp: fused.last_update_s,
        });
        count += 1;
    }
    count
}

fn turn_from_flank(side: FlankSide) -> TurnDirection {
    match side {
        FlankSide::Left => TurnDirection::Left,
        FlankSide::Right => TurnDirection::Right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_platform::MockAdapter;
    use openfang_types::config::{
        InterventionConfig, InterventionMode, InterventionRule, PlatformConfig, PlatformMode,
    };
    use openfang_types::platform::{
        Affiliation, CcaRole, PlatformCommand, PlatformState, SensorState, SensorType, Track,
        WorldSnapshot,
    };
    use openfang_types::tactical::{CommandPriority, IntentSource};
    use openfang_types::umaa::{PlatformLimits, RulesOfEngagement};

    fn restrictions() -> Arc<OpRestrictionsManager> {
        Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement::default(),
            PlatformLimits::default(),
        ))
    }

    fn restrictions_with_weapons_tight() -> Arc<OpRestrictionsManager> {
        Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement {
                weapon_release_authority: openfang_types::umaa::WeaponReleaseLevel::WeaponsTight,
                ..Default::default()
            },
            PlatformLimits::default(),
        ))
    }

    fn caps() -> PlatformCapabilities {
        PlatformCapabilities {
            supports_motion_control: true,
            supports_sensor_control: true,
            supports_weapon_control: true,
            supports_jammer_control: true,
            supports_comm_control: true,
            supports_uav_launch_recovery: true,
            supports_formation_control: true,
            supports_handoff: true,
            max_platforms: 64,
            supports_simulation: true,
            supports_hardware: false,
        }
    }

    async fn loop_with_mock() -> (
        PlatformControlLoop,
        Arc<std::sync::Mutex<Vec<PlatformCommand>>>,
    ) {
        let registry = Arc::new(AdapterRegistry::new());
        let mock = MockAdapter::new("primary");
        let log = mock.sent_handle();
        registry.set_primary(Box::new(mock));
        registry.connect_all().await.unwrap();
        let lp = PlatformControlLoop::new(
            registry,
            restrictions(),
            Arc::new(AuditLog::new()),
            caps(),
            "self",
            20.0,
            256,
            2,
            30.0,
        );
        (lp, log)
    }

    fn snapshot_with_track() -> WorldSnapshot {
        let mut p = PlatformState::minimal("self");
        p.affiliation = Affiliation::Blue;
        p.tracks = vec![Track {
            track_id: "trk-1".into(),
            target_name: String::new(),
            classification: "uav".into(),
            affiliation: Affiliation::Red,
            iff: "foe".into(),
            position_lla: Some((30.1, 120.1, 0.0)),
            heading_deg: Some(90.0),
            speed_ms: Some(30.0),
            range_m: Some(10_000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.8,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        }];
        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![p],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn snapshot_with_active_radar() -> WorldSnapshot {
        let mut p = PlatformState::minimal("self");
        p.affiliation = Affiliation::Blue;
        p.onboard_sensors = vec![SensorState {
            sensor_id: "surf_radar".into(),
            sensor_type: SensorType::Radar,
            mode: "active".into(),
            frequency_hz: None,
            bandwidth_hz: None,
            azimuth_fov_deg: None,
            elevation_fov_deg: None,
            range_max_m: Some(30_000.0),
            damage: 0.0,
            host_platform_id: "self".into(),
        }];
        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![p],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    #[tokio::test]
    async fn explicit_sensor_intent_overrides_sms_emcon_shutdown() {
        let registry = Arc::new(AdapterRegistry::new());
        registry.set_primary(Box::new(
            MockAdapter::new("primary").with_snapshot(snapshot_with_active_radar()),
        ));
        registry.connect_all().await.unwrap();
        let mut lp = PlatformControlLoop::new(
            registry,
            restrictions(),
            Arc::new(AuditLog::new()),
            caps(),
            "self",
            20.0,
            256,
            2,
            30.0,
        );
        lp.set_own_role(CcaRole::Recon);
        lp.submit_intent(CandidateIntent::new(
            PlatformCommand::SensorOn {
                platform_id: "self".into(),
                sensor_id: "surf_radar".into(),
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "operator".into(),
            },
            0.0,
            "operator radar on",
        ));

        let report = lp.step().await;

        assert_eq!(
            report.sms_intents, 0,
            "explicit operator sensor command should suppress SMS SensorOff"
        );
    }

    #[tokio::test]
    async fn step_updates_fusion_and_track_manager_views() {
        let registry = Arc::new(AdapterRegistry::new());
        registry.set_primary(Box::new(
            MockAdapter::new("primary").with_snapshot(snapshot_with_track()),
        ));
        registry.connect_all().await.unwrap();
        let mut lp = PlatformControlLoop::new(
            registry,
            restrictions(),
            Arc::new(AuditLog::new()),
            caps(),
            "self",
            20.0,
            256,
            2,
            30.0,
        );

        let report = lp.step().await;

        assert_eq!(report.fused_tracks, 1);
        assert_eq!(report.track_correlations, 1);
        assert_eq!(
            lp.latest_fusion().unwrap().fused_tracks[0].track_id,
            "trk-1"
        );
    }

    #[tokio::test]
    async fn motion_intent_flows_to_adapter() {
        let (mut lp, log) = loop_with_mock().await;
        lp.submit_intent(CandidateIntent::new(
            PlatformCommand::SetHeading {
                platform_id: "self".into(),
                heading_deg: 90.0,
                speed_ms: Some(10.0),
                turn_direction: None,
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "na".into(),
            },
            0.0,
            "turn",
        ));
        let report = lp.step().await;
        assert!(report.polled);
        assert_eq!(
            report.pipeline.dispatched, 1,
            "motion command should be dispatched"
        );
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn workflow_motion_intent_updates_task_execution_record() {
        let (mut lp, _log) = loop_with_mock().await;
        lp.submit_intent(CandidateIntent::new(
            PlatformCommand::SetHeading {
                platform_id: "self".into(),
                heading_deg: 90.0,
                speed_ms: Some(10.0),
                turn_direction: None,
            },
            CommandPriority::Normal,
            IntentSource::Workflow {
                workflow_id: "patrol:default".into(),
            },
            0.0,
            "turn",
        ));

        let report = lp.step().await;

        assert_eq!(report.pipeline.dispatched, 1);
        let records = lp.pipeline_mut().task_execution_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].task_id, "patrol:default");
    }

    #[tokio::test]
    async fn weapon_intent_becomes_pending_not_dispatched() {
        let (mut lp, log) = loop_with_mock().await;
        lp.submit_intent(CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "w1".into(),
                track_id: "t1".into(),
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "fca".into(),
            },
            0.0,
            "engage",
        ));
        let report = lp.step().await;
        // Iron Law: a weapon intent is never auto-dispatched — it is either held
        // pending quorum approval or rejected by ROE, but it never reaches the
        // adapter without passing the engagement pipeline.
        assert_eq!(report.pipeline.dispatched, 0);
        assert!(
            report.pipeline.pending + report.pipeline.rejected >= 1,
            "weapon intent must be gated (pending or rejected)"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            0,
            "no weapon may bypass the gate"
        );
    }

    #[tokio::test]
    async fn approved_launch_waits_for_bda_before_second_round() {
        let registry = Arc::new(AdapterRegistry::new());
        let mock = MockAdapter::new("primary");
        let log = mock.sent_handle();
        registry.set_primary(Box::new(mock));
        registry.connect_all().await.unwrap();
        let mut lp = PlatformControlLoop::new(
            registry,
            restrictions_with_weapons_tight(),
            Arc::new(AuditLog::new()),
            caps(),
            "self",
            20.0,
            256,
            1,
            30.0,
        );

        for issued_at in [0.0, 1.0] {
            lp.submit_intent(CandidateIntent::new(
                PlatformCommand::FireAtTarget {
                    platform_id: "self".into(),
                    weapon_id: "loiter_wave3".into(),
                    track_id: "self:4".into(),
                },
                CommandPriority::Normal,
                IntentSource::Llm {
                    agent_id: "fca".into(),
                },
                issued_at,
                "engage same target",
            ));
            let report = lp.step().await;
            assert_eq!(report.pipeline.dispatched, 0);
            assert_eq!(report.pipeline.pending, 1);
            let approval_id = lp
                .pipeline_mut()
                .pending_ids()
                .into_iter()
                .next()
                .expect("pending engagement id");
            assert!(lp.pipeline_mut().sign(&approval_id, "operator").is_some());
            let launched = lp.pipeline_mut().launch_if_ready(&approval_id).await;
            if issued_at == 0.0 {
                assert!(launched, "first authorized round may launch");
                assert_eq!(log.lock().unwrap().len(), 1);
            } else {
                assert!(
                    !launched,
                    "second round at same target must wait for BDA evidence"
                );
                assert_eq!(
                    log.lock().unwrap().len(),
                    1,
                    "second round must not reach the adapter"
                );
            }
        }
    }

    #[tokio::test]
    async fn idle_step_dispatches_nothing() {
        let (mut lp, log) = loop_with_mock().await;
        let report = lp.step().await;
        assert!(report.polled);
        assert_eq!(report.pipeline.dispatched, 0);
        assert_eq!(log.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn from_config_wires_authorized_target_intervention_into_fast_gate() {
        let registry = Arc::new(AdapterRegistry::new());
        let mock = MockAdapter::new("primary");
        let log = mock.sent_handle();
        registry.set_primary(Box::new(mock));
        registry.connect_all().await.unwrap();
        let target_registry =
            Arc::new(openfang_runtime::target_authorization::TargetAuthorizationRegistry::new());
        let cfg = PlatformConfig {
            mode: PlatformMode::Simulation,
            own_platform_id: "self".into(),
            intervention: InterventionConfig {
                rules: vec![InterventionRule {
                    stage: vec!["weapon_release".into()],
                    platform_ids: vec!["self".into()],
                    command_classes: vec!["weapon".into()],
                    sources: vec!["llm".into()],
                    mode: InterventionMode::AuthorizedTarget,
                    quorum: 1,
                    window_s: 30.0,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut lp = PlatformControlLoop::from_config(
            registry,
            &cfg,
            restrictions_with_weapons_tight(),
            Arc::new(AuditLog::new()),
            caps(),
            Arc::clone(&target_registry),
            Arc::new(openfang_runtime::mission_approval::MissionApprovalRegistry::new()),
        );

        lp.submit_intent(CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "w1".into(),
                track_id: "t1".into(),
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "fca".into(),
            },
            0.0,
            "engage",
        ));
        let report = lp.step().await;
        assert_eq!(report.pipeline.dispatched, 0);
        assert_eq!(report.pipeline.pending, 1);

        target_registry.authorize("self", "t1", "operator", 1.0);
        lp.submit_intent(CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "w1".into(),
                track_id: "t1".into(),
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "fca".into(),
            },
            1.0,
            "engage after target auth",
        ));
        let report = lp.step().await;
        assert_eq!(report.pipeline.dispatched, 1);
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    /// Regression for the "授权后实体未开火" report: under the fail-safe default
    /// ROE (`WeaponsHold`), the final SPGS interlock rejects every weapon command
    /// even when the target is authorized and the intervention rule matches. This
    /// is exactly why hard-coding `RulesOfEngagement::default()` (WeaponsHold) in
    /// the kernel made the whole fire chain dead — nothing could ever dispatch.
    #[tokio::test]
    async fn weapons_hold_blocks_authorized_fire_even_with_authorized_target() {
        let registry = Arc::new(AdapterRegistry::new());
        let mock = MockAdapter::new("primary");
        let log = mock.sent_handle();
        registry.set_primary(Box::new(mock));
        registry.connect_all().await.unwrap();
        let target_registry =
            Arc::new(openfang_runtime::target_authorization::TargetAuthorizationRegistry::new());
        let cfg = PlatformConfig {
            mode: PlatformMode::Simulation,
            own_platform_id: "self".into(),
            intervention: InterventionConfig {
                rules: vec![InterventionRule {
                    stage: vec!["weapon_release".into()],
                    platform_ids: vec![],
                    command_classes: vec!["weapon".into()],
                    sources: vec!["llm".into()],
                    mode: InterventionMode::AuthorizedTarget,
                    quorum: 1,
                    window_s: 30.0,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        // ROE = WeaponsHold (the kernel's old hard-coded default).
        let mut lp = PlatformControlLoop::from_config(
            registry,
            &cfg,
            restrictions(),
            Arc::new(AuditLog::new()),
            caps(),
            Arc::clone(&target_registry),
            Arc::new(openfang_runtime::mission_approval::MissionApprovalRegistry::new()),
        );

        // Authorize the target up-front, then submit a fire intent.
        target_registry.authorize("self", "t1", "operator", 0.0);
        lp.submit_intent(CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "w1".into(),
                track_id: "t1".into(),
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "fca".into(),
            },
            0.0,
            "engage authorized target under weapons_hold",
        ));
        let report = lp.step().await;
        // SPGS interlock blocks the shot outright — neither dispatched nor pending.
        assert_eq!(
            report.pipeline.dispatched, 0,
            "weapons_hold must block dispatch"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            0,
            "no weapon may reach the adapter under WeaponsHold"
        );
    }
}
