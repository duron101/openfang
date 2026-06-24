//! TacticalPipeline — the single end-to-end path from intents to actuation.
//!
//! This is the only place the deconfliction, gating, weapon-authorization, and
//! adapter-dispatch stages are wired together. It enforces the Iron Laws:
//!
//! - Producers submit [`CandidateIntent`]s only.
//! - The [`ActionComposer`] deconflicts; the [`CommandGate`] is the sole arbiter
//!   of what becomes a dispatchable command.
//! - Weapon-class intents deferred by the gate become asynchronous
//!   [`WeaponEngagement`]s; they never block the tick.
//! - Only gate-approved commands are routed to the [`AdapterRegistry`].
//! - Every approve / reject / pending / expire / launch is audited.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use openfang_platform::AdapterRegistry;
use openfang_types::platform::{PlatformCapabilities, PlatformCommand, WorldSnapshot};
use openfang_types::tactical::{CandidateIntent, IntentSource, TimeSource};
use openfang_types::umaa::WeaponReleaseLevel;
use std::collections::HashMap;

use openfang_runtime::action_composer::ActionComposer;
use openfang_runtime::audit::{AuditAction, AuditLog};
use openfang_runtime::command_gate::{
    command_summary, ApprovalPolicy, CommandGate, GateContext, WeaponApproval,
};
use openfang_runtime::engagement_guard::EngagementGuard;
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::task_execution::{
    TaskExecutionRecord, TaskExecutionRegistry, TaskExecutionStatus,
};
use openfang_runtime::weapon_engagement::{
    EngagementSnapshot, EngagementState, WeaponEngagementManager,
};
use openfang_types::config::AutonomyModeProfile;
use std::sync::RwLock;

/// Summary of one pipeline tick.
#[derive(Debug, Default, Clone)]
pub struct TickReport {
    /// Commands routed to the adapter this tick.
    pub dispatched: usize,
    /// Intents rejected by the gate this tick.
    pub rejected: usize,
    /// Weapon intents deferred to asynchronous approval this tick.
    pub pending: usize,
    /// Engagements that expired this tick.
    pub expired: usize,
}

/// The composed tactical command pipeline.
pub struct TacticalPipeline {
    composer: ActionComposer,
    gate: CommandGate,
    /// Fire-once de-dup + decision-time weapon (ammo/range) checks. Without this
    /// the fast loop replays the standing-plan `FireAtTarget` every tick.
    engagement_guard: EngagementGuard,
    engagements: WeaponEngagementManager,
    registry: Arc<AdapterRegistry>,
    restrictions: Arc<OpRestrictionsManager>,
    audit: Arc<AuditLog>,
    capabilities: PlatformCapabilities,
    autonomy_profile: Option<Arc<RwLock<AutonomyModeProfile>>>,
    task_execution: TaskExecutionRegistry,
    /// Last value dispatched on each idempotent command "lane" (e.g.
    /// `heading:self`, `sensor:self:eoir`). The fast loop replays the whole
    /// standing plan every tick; weapon replays are caught by
    /// [`EngagementGuard`], but idempotent state-setters (heading, speed,
    /// sensor mode, outside-control) used to pass through unconditionally and
    /// flood the dispatch path ~15×/s. We suppress a command byte-identical to
    /// the last one dispatched on its lane; a *changed* value re-dispatches once.
    dispatched_state: HashMap<String, String>,
}

fn is_replay_source(source: &IntentSource) -> bool {
    matches!(
        source,
        IntentSource::Workflow { .. } | IntentSource::Dcc { .. }
    )
}

/// Stable "lane" identifier for idempotent, state-setting platform commands.
///
/// A lane represents one control axis on one platform (e.g. its heading, or one
/// sensor's mode). Two commands on the same lane supersede each other, so only
/// the latest distinct value needs to reach the simulator. Commands that are
/// transient (one-shot effects like fires, messages, UAV launch) or are guarded
/// elsewhere (weapon fire-once via [`EngagementGuard`], and `WeaponSafeAll`,
/// which must always re-assert for safety) return `None` and are never deduped.
fn idempotent_lane_key(cmd: &PlatformCommand) -> Option<String> {
    use PlatformCommand::*;
    match cmd {
        SetHeading { platform_id, .. } => Some(format!("heading:{platform_id}")),
        SetSpeed { platform_id, .. } => Some(format!("speed:{platform_id}")),
        SetAltitude { platform_id, .. } => Some(format!("altitude:{platform_id}")),
        GotoLocation { platform_id, .. } => Some(format!("goto:{platform_id}")),
        FollowRoute { platform_id, .. } => Some(format!("route:{platform_id}")),
        SensorOn {
            platform_id,
            sensor_id,
        }
        | SensorOff {
            platform_id,
            sensor_id,
        }
        | SensorSetMode {
            platform_id,
            sensor_id,
            ..
        } => Some(format!("sensor:{platform_id}:{sensor_id}")),
        JamStart {
            platform_id,
            jammer_id,
            ..
        }
        | JamStop {
            platform_id,
            jammer_id,
        }
        | JamSetMode {
            platform_id,
            jammer_id,
            ..
        } => Some(format!("jam:{platform_id}:{jammer_id}")),
        CommOn { platform_id } | CommOff { platform_id } => Some(format!("comm:{platform_id}")),
        SetOutsideControl { platform_id } | ReleaseOutsideControl { platform_id } => {
            Some(format!("control:{platform_id}"))
        }
        _ => None,
    }
}

impl TacticalPipeline {
    /// Build the standard safe pipeline.
    pub fn new(
        registry: Arc<AdapterRegistry>,
        restrictions: Arc<OpRestrictionsManager>,
        audit: Arc<AuditLog>,
        capabilities: PlatformCapabilities,
        weapon_quorum: u32,
        approval_window_s: f64,
    ) -> Self {
        let approval: Arc<dyn ApprovalPolicy> = Arc::new(WeaponApproval::default());
        Self::new_with_approval(
            registry,
            restrictions,
            audit,
            capabilities,
            weapon_quorum,
            approval_window_s,
            approval,
        )
    }

    /// Build a pipeline with a caller-supplied approval policy.
    pub fn new_with_approval(
        registry: Arc<AdapterRegistry>,
        restrictions: Arc<OpRestrictionsManager>,
        audit: Arc<AuditLog>,
        capabilities: PlatformCapabilities,
        weapon_quorum: u32,
        approval_window_s: f64,
        approval: Arc<dyn ApprovalPolicy>,
    ) -> Self {
        let gate = CommandGate::standard(audit.clone(), approval, restrictions.clone());
        Self {
            composer: ActionComposer::new(),
            gate,
            engagement_guard: EngagementGuard::default(),
            engagements: WeaponEngagementManager::new(weapon_quorum, approval_window_s),
            registry,
            restrictions,
            audit,
            capabilities,
            autonomy_profile: None,
            task_execution: TaskExecutionRegistry::new(),
            dispatched_state: HashMap::new(),
        }
    }

    /// Build a pipeline with a caller-supplied approval policy *and* an
    /// autonomy-mode profile inserted into the gate. The profile is shared so
    /// hot-reload can swap it without rebuilding the pipeline.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_autonomy(
        registry: Arc<AdapterRegistry>,
        restrictions: Arc<OpRestrictionsManager>,
        audit: Arc<AuditLog>,
        capabilities: PlatformCapabilities,
        weapon_quorum: u32,
        approval_window_s: f64,
        approval: Arc<dyn ApprovalPolicy>,
        autonomy_profile: Arc<RwLock<AutonomyModeProfile>>,
    ) -> Self {
        let gate = CommandGate::standard_with_autonomy(
            audit.clone(),
            approval,
            restrictions.clone(),
            Arc::clone(&autonomy_profile),
        );
        Self {
            composer: ActionComposer::new(),
            gate,
            engagement_guard: EngagementGuard::default(),
            engagements: WeaponEngagementManager::new(weapon_quorum, approval_window_s),
            registry,
            restrictions,
            audit,
            capabilities,
            autonomy_profile: Some(Arc::clone(&autonomy_profile)),
            task_execution: TaskExecutionRegistry::new(),
            dispatched_state: HashMap::new(),
        }
    }

    /// Run one tick: deconflict, gate, dispatch approved, defer weapons, expire.
    pub async fn tick(
        &mut self,
        new_intents: Vec<CandidateIntent>,
        snapshot: Option<&WorldSnapshot>,
        time: &dyn TimeSource,
    ) -> TickReport {
        let composed = self.composer.compose(new_intents);

        let result = {
            let autonomy_profile_id = self
                .autonomy_profile
                .as_ref()
                .map(|profile| profile.read().unwrap_or_else(|e| e.into_inner()).id.clone());
            let ctx = GateContext {
                capabilities: &self.capabilities,
                snapshot,
                autonomy_profile: autonomy_profile_id.as_deref(),
            };
            self.gate.evaluate_batch(composed, &ctx)
        };

        let mut report = TickReport {
            rejected: result.rejected.len(),
            pending: result.pending.len(),
            ..Default::default()
        };

        // Defer weapon intents awaiting approval into the engagement state machine.
        let now = time.now_secs();
        for (intent, approval_id) in result.pending {
            self.task_execution.record_intent(
                &intent,
                TaskExecutionStatus::Pending,
                now,
                format!("approval_id={approval_id}"),
            );
            self.engagements.open(approval_id, intent.command, now);
        }

        // Expire stale engagements (non-blocking) and audit each expiry.
        let expired = self.engagements.tick(time);
        report.expired = expired.expired.len();
        for id in &expired.expired {
            self.audit.record(
                "weapon_engagement",
                AuditAction::CapabilityCheck,
                format!("engagement {id}"),
                "expired",
            );
        }

        for approval_id in self.engagements.approved_ids() {
            if self
                .launch_if_ready_with_snapshot(&approval_id, snapshot)
                .await
            {
                report.dispatched += 1;
            }
        }

        // Fire-once de-dup + decision-time weapon checks. The fast loop replays
        // the standing plan every tick; without this a single FireAtTarget would
        // be re-dispatched ~20×/s. Non-weapon commands always pass.
        let mut approved_intents = Vec::with_capacity(result.approved.len());
        for intent in result.approved {
            match self.engagement_guard.check(&intent.command, snapshot, now) {
                Ok(()) => approved_intents.push(intent),
                Err(reason) => {
                    self.task_execution.record_intent(
                        &intent,
                        TaskExecutionStatus::Rejected,
                        now,
                        format!("fire suppressed: {}", reason.as_str()),
                    );
                    self.audit.record(
                        "weapon_engagement",
                        AuditAction::CapabilityCheck,
                        command_summary(&intent.command),
                        format!("suppressed:{}", reason.as_str()),
                    );
                }
            }
        }

        // Idempotent non-weapon de-dup for replaying sources only. Workflow/DCC
        // lanes may re-derive the same persistent state-setter every tick; LLM,
        // operator, and external intents are explicit submissions and must be
        // allowed to re-send the same value after gate/profile recovery.
        approved_intents.retain(|intent| match idempotent_lane_key(&intent.command) {
            Some(_) if !is_replay_source(&intent.source) => true,
            Some(lane) => {
                let sig = format!("{:?}", intent.command);
                self.dispatched_state.get(&lane).map(String::as_str) != Some(sig.as_str())
            }
            None => true,
        });

        // Dispatch gate-approved commands to the adapter.
        if !approved_intents.is_empty() {
            let commands: Vec<_> = approved_intents
                .iter()
                .map(|intent| intent.command.clone())
                .collect();
            match self.registry.route_commands(&commands).await {
                Ok(r) => {
                    report.dispatched += r.accepted as usize;
                    for intent in &approved_intents {
                        // Start the cooldown window for fires that actually went out.
                        self.engagement_guard
                            .record_fire(&intent.command, snapshot, now);
                        // Remember the value now in effect on this idempotent lane
                        // so identical replays next tick are suppressed.
                        if let Some(lane) = idempotent_lane_key(&intent.command) {
                            self.dispatched_state
                                .insert(lane, format!("{:?}", intent.command));
                        }
                        self.task_execution.record_intent(
                            intent,
                            TaskExecutionStatus::Dispatched,
                            now,
                            "adapter accepted",
                        );
                    }
                }
                Err(e) => {
                    self.audit.record(
                        "adapter",
                        AuditAction::CapabilityCheck,
                        "route_commands",
                        format!("error: {e}"),
                    );
                }
            }
        }

        for (intent, decision) in result.rejected {
            self.task_execution.record_intent(
                &intent,
                TaskExecutionStatus::Rejected,
                now,
                format!("{decision:?}"),
            );
        }

        report
    }

    /// Replace the standing (slow-loop) plan.
    pub fn set_active_plan(&mut self, intents: Vec<CandidateIntent>) {
        self.composer.set_active_plan(intents);
    }

    /// Set the per-engagement re-fire cooldown (seconds). Suppresses identical
    /// `(platform, weapon, track)` fires replayed by the fast loop within the
    /// window.
    pub fn set_engagement_cooldown_secs(&mut self, secs: f64) {
        self.engagement_guard.set_cooldown_secs(secs);
    }

    /// Set per-weapon/type cooldown overrides for weapon re-attack decisions.
    pub fn set_weapon_cooldowns_secs(&mut self, cooldowns: std::collections::HashMap<String, f64>) {
        self.engagement_guard.set_weapon_cooldowns_secs(cooldowns);
    }

    /// Release the fire-once lock for a `(platform, track)` engagement so the
    /// operator can re-designate a target for a fresh strike. Called from the
    /// target-authorization path: a deliberate (re-)authorization is the signal
    /// that a new strike against this target is intended.
    pub fn release_engagement(&mut self, platform_id: &str, track_id: &str) {
        self.engagement_guard
            .release_engagement(platform_id, track_id);
    }

    /// Add a signature to a pending weapon engagement.
    pub fn sign(&mut self, approval_id: &str, signer: &str) -> Option<EngagementState> {
        self.engagements.add_signature(approval_id, signer)
    }

    /// Reject a pending weapon engagement.
    pub fn reject_engagement(&mut self, approval_id: &str) -> Option<EngagementState> {
        let st = self.engagements.reject(approval_id);
        if st.is_some() {
            self.audit.record(
                "weapon_engagement",
                AuditAction::CapabilityCheck,
                format!("engagement {approval_id}"),
                "rejected",
            );
        }
        st
    }

    /// Arm and launch an approved engagement, applying a final ROE interlock and
    /// dispatching the weapon command. Returns true if the weapon was launched.
    pub async fn launch_if_ready(&mut self, approval_id: &str) -> bool {
        self.launch_if_ready_with_snapshot(approval_id, None).await
    }

    /// Like [`Self::launch_if_ready`], but with the latest world snapshot so the
    /// EngagementGuard can treat a post-strike observation as BDA before allowing
    /// a second round at the same target.
    pub async fn launch_if_ready_with_snapshot(
        &mut self,
        approval_id: &str,
        snapshot: Option<&WorldSnapshot>,
    ) -> bool {
        if self.engagements.state(approval_id) != Some(EngagementState::Approved) {
            return false;
        }
        // Final safety interlock: never release under WeaponsHold even if approved.
        if self.restrictions.get_roe().weapon_release_authority == WeaponReleaseLevel::WeaponsHold {
            self.audit.record(
                "weapon_engagement",
                AuditAction::CapabilityCheck,
                format!("engagement {approval_id}"),
                "blocked:weapons_hold",
            );
            return false;
        }
        self.engagements.arm(approval_id);
        let Some(cmd) = self.engagements.launch_command(approval_id) else {
            return false;
        };
        let summary = command_summary(&cmd);
        let now = wall_clock_secs();
        if let Err(reason) = self.engagement_guard.check(&cmd, snapshot, now) {
            self.engagements.abort(approval_id);
            self.audit.record(
                "weapon_engagement",
                AuditAction::CapabilityCheck,
                format!("engagement {approval_id} {summary}"),
                format!("blocked:{}", reason.as_str()),
            );
            return false;
        }
        let dispatched = self
            .registry
            .route_commands(std::slice::from_ref(&cmd))
            .await
            .is_ok();
        if dispatched {
            self.engagement_guard.record_fire(&cmd, snapshot, now);
            self.engagements.mark_launched(approval_id);
        } else {
            self.engagements.abort(approval_id);
        }
        self.audit.record(
            "weapon_engagement",
            AuditAction::CapabilityCheck,
            format!("engagement {approval_id} {summary}"),
            if dispatched {
                "launched"
            } else {
                "launch_failed"
            },
        );
        dispatched
    }

    /// Current state of an engagement, if tracked.
    pub fn engagement_state(&self, approval_id: &str) -> Option<EngagementState> {
        self.engagements.state(approval_id)
    }

    /// Approval ids of engagements still awaiting signatures.
    pub fn pending_ids(&self) -> Vec<String> {
        self.engagements.pending_ids()
    }

    /// All tracked engagement lifecycle snapshots, including approved/launched.
    pub fn engagement_snapshots(&self) -> Vec<EngagementSnapshot> {
        self.engagements.snapshots()
    }

    /// Snapshot of workflow task execution records.
    pub fn task_execution_records(&self) -> Vec<TaskExecutionRecord> {
        self.task_execution.list()
    }

    /// Shared audit log handle.
    pub fn audit(&self) -> Arc<AuditLog> {
        self.audit.clone()
    }
}

fn wall_clock_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod dedup_tests {
    use super::*;
    use openfang_platform::MockAdapter;
    use openfang_types::tactical::{CommandPriority, IntentSource, WallClock};
    use openfang_types::umaa::{PlatformLimits, RulesOfEngagement};
    use std::sync::Mutex;

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

    async fn pipeline_with_mock() -> (TacticalPipeline, Arc<Mutex<Vec<PlatformCommand>>>) {
        let registry = Arc::new(AdapterRegistry::new());
        let mock = MockAdapter::new("primary");
        let log = mock.sent_handle();
        registry.set_primary(Box::new(mock));
        registry.connect_all().await.unwrap();
        let restrictions = Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement::default(),
            PlatformLimits::default(),
        ));
        let pipeline = TacticalPipeline::new(
            registry,
            restrictions,
            Arc::new(AuditLog::new()),
            caps(),
            2,
            30.0,
        );
        (pipeline, log)
    }

    fn heading(deg: f64) -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::SetHeading {
                platform_id: "self".into(),
                heading_deg: deg,
                speed_ms: None,
                turn_direction: None,
            },
            CommandPriority::Normal,
            IntentSource::Workflow {
                workflow_id: "patrol:default".into(),
            },
            0.0,
            "standing heading",
        )
    }

    /// The standing plan is replayed verbatim every tick. An unchanged
    /// idempotent order (heading) must dispatch exactly once, not once per tick.
    #[tokio::test]
    async fn unchanged_standing_heading_dispatches_once() {
        let (mut pipeline, log) = pipeline_with_mock().await;
        let clock = WallClock;
        pipeline.set_active_plan(vec![heading(90.0)]);

        let first = pipeline.tick(vec![], None, &clock).await;
        assert_eq!(first.dispatched, 1, "first heading must reach the adapter");

        // Replay the same standing plan for several ticks — all suppressed.
        for _ in 0..5 {
            let r = pipeline.tick(vec![], None, &clock).await;
            assert_eq!(r.dispatched, 0, "unchanged heading must be suppressed");
        }
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "adapter should only have seen the heading once"
        );
    }

    /// A genuinely changed value re-dispatches exactly once, then holds again.
    #[tokio::test]
    async fn changed_standing_heading_redispatches_once() {
        let (mut pipeline, log) = pipeline_with_mock().await;
        let clock = WallClock;

        pipeline.set_active_plan(vec![heading(90.0)]);
        assert_eq!(pipeline.tick(vec![], None, &clock).await.dispatched, 1);
        assert_eq!(pipeline.tick(vec![], None, &clock).await.dispatched, 0);

        // Operator/slow-loop changes the heading: must go out once.
        pipeline.set_active_plan(vec![heading(45.0)]);
        assert_eq!(
            pipeline.tick(vec![], None, &clock).await.dispatched,
            1,
            "changed heading must re-dispatch"
        );
        assert_eq!(
            pipeline.tick(vec![], None, &clock).await.dispatched,
            0,
            "new heading then holds"
        );

        assert_eq!(log.lock().unwrap().len(), 2, "two distinct headings sent");
    }
}
