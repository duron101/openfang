//! Slow cognition pipeline orchestration.
//!
//! Chains rule-based cognition → planning (with optional LLM refinement) →
//! mission decomposition → playbook scheduling. The `mission_approval`
//! checkpoint is authoritative: only an **Approved** plan emits actuation
//! intents. A **Pending** plan exposes the proposed tasks/tactics for operator
//! visibility but withholds all intents; a **Denied** plan emits nothing.

use std::sync::{Arc, RwLock};

use openfang_types::cognition::{CommanderIntent, SituationAssessment, Tactic, Task};
use openfang_types::config::PlatformControlPolicy;
use openfang_types::platform::WorldSnapshot;
use openfang_types::tactical::CandidateIntent;
use openfang_types::umaa::MissionConfig;

use crate::cognition::CognitionEngine;
use crate::intervention::InterventionGate;
use crate::planning::{MissionPlanRefiner, Planner, PlanningOutcome, RefineContext};
use crate::playbook_scheduler::{MissionDecomposer, PlaybookScheduler};

pub struct CognitivePipeline {
    cognition: CognitionEngine,
    planner: Planner,
    decomposer: MissionDecomposer,
    scheduler: PlaybookScheduler,
    control_policy: PlatformControlPolicy,
}

impl CognitivePipeline {
    pub fn new(intervention: Arc<RwLock<InterventionGate>>) -> Self {
        Self {
            cognition: CognitionEngine::default(),
            planner: Planner::new(intervention),
            decomposer: MissionDecomposer::new(),
            scheduler: PlaybookScheduler::new(),
            control_policy: PlatformControlPolicy::default(),
        }
    }

    /// Apply the configured control scope (side, entity allow-list, controller id).
    pub fn with_control_policy(mut self, policy: PlatformControlPolicy) -> Self {
        self.apply_control_policy(policy);
        self
    }

    /// Hot-swap the control policy in place (runtime change of controlled side /
    /// threat side / entity allow-list) without rebuilding the pipeline.
    pub fn apply_control_policy(&mut self, policy: PlatformControlPolicy) {
        self.planner
            .set_controlled_platforms(policy.controlled_platforms.clone());
        // S4: give the decomposer the binding pool so role slots resolve to
        // concrete platform ids (and `"self"` resolves to the own platform).
        self.decomposer.set_binding(
            policy.controlled_platforms.clone(),
            policy.own_platform_id.clone(),
        );
        self.cognition = CognitionEngine::new(policy.clone());
        self.control_policy = policy;
    }

    /// Restrict slow-loop tasking to an allow-list of platform ids (empty = no
    /// limit within the configured controlled side). Prefer
    /// [`Self::with_control_policy`] when wiring from config.
    pub fn with_controlled_platforms(mut self, platforms: Vec<String>) -> Self {
        self.planner.set_controlled_platforms(platforms.clone());
        self.decomposer
            .set_binding(platforms, self.control_policy.own_platform_id.clone());
        self
    }

    pub fn run_once(&self, snapshot: &WorldSnapshot, mission: MissionConfig) -> CognitiveReport {
        self.run_once_with_intent(snapshot, mission, None)
    }

    pub fn run_once_with_intent(
        &self,
        snapshot: &WorldSnapshot,
        mission: MissionConfig,
        intent: Option<CommanderIntent>,
    ) -> CognitiveReport {
        let assessment = self.cognition.assess(snapshot);
        let outcome = self.planner.plan(&assessment, intent, mission);
        self.assemble(assessment, outcome, snapshot.timestamp)
    }

    /// As [`Self::run_once_refined`] but回灌s the unified SMS fusion picture into
    /// the cognition step so WMS target allocation reasons about the same
    /// Kalman-confirmed threats the cerebellum services act on. Passing an empty
    /// `fused_tracks` slice reproduces the raw-only behaviour exactly.
    pub async fn run_once_refined_with_fused(
        &self,
        snapshot: &WorldSnapshot,
        mission: MissionConfig,
        intent: Option<CommanderIntent>,
        refiner: Option<&dyn MissionPlanRefiner>,
        fused_tracks: &[crate::sensor_fusion::FusedTrack],
    ) -> CognitiveReport {
        let assessment = self.cognition.assess_with_fused(snapshot, fused_tracks);
        self.refine_assess(snapshot, mission, intent, refiner, assessment)
            .await
    }

    /// Async variant that runs the deterministic baseline, optionally lets a
    /// refiner narrow/re-prioritize it, then applies the approval checkpoint.
    pub async fn run_once_refined(
        &self,
        snapshot: &WorldSnapshot,
        mission: MissionConfig,
        intent: Option<CommanderIntent>,
        refiner: Option<&dyn MissionPlanRefiner>,
    ) -> CognitiveReport {
        let assessment = self.cognition.assess(snapshot);
        self.refine_assess(snapshot, mission, intent, refiner, assessment)
            .await
    }

    /// Shared refinement + gating tail for the `run_once_refined*` family. Takes
    /// a pre-computed [`SituationAssessment`] so callers can choose a raw-only or
    /// fusion-回灌 assessment without duplicating the LLM-refine plumbing.
    async fn refine_assess(
        &self,
        snapshot: &WorldSnapshot,
        mission: MissionConfig,
        intent: Option<CommanderIntent>,
        refiner: Option<&dyn MissionPlanRefiner>,
        assessment: SituationAssessment,
    ) -> CognitiveReport {
        let baseline = self.planner.baseline(&assessment, intent.as_ref(), mission);
        let baseline_allocation_count = baseline.allocations.len();
        let mut llm_refined = false;
        let mut authorization_proposals = Vec::new();
        let refined = match refiner {
            Some(refiner) => {
                let ctx = RefineContext {
                    assessment: assessment.clone(),
                    intent: intent.clone(),
                    baseline: baseline.clone(),
                };
                match refiner.refine(ctx).await {
                    Some(refinement) => {
                        llm_refined = true;
                        // Resolve fire-authorization proposals against the baseline
                        // before it is consumed. The kernel decides whether to honor
                        // them (config + ROE gated); apply_refinement bakes weapon
                        // employment + narrowing.
                        authorization_proposals =
                            crate::planning::authorization_proposals(&baseline, &refinement);
                        crate::planning::apply_refinement_with_policy(
                            baseline,
                            &refinement,
                            &self.control_policy,
                            Some(snapshot),
                        )
                    }
                    None => baseline,
                }
            }
            None => baseline,
        };
        let outcome = self.planner.gate(refined);
        let mut report = self.assemble(assessment, outcome, snapshot.timestamp);
        report.llm_refine_enabled = refiner.is_some();
        report.llm_refined = llm_refined;
        report.baseline_allocation_count = baseline_allocation_count;
        report.authorization_proposals = authorization_proposals;
        report
    }

    fn assemble(
        &self,
        assessment: SituationAssessment,
        outcome: PlanningOutcome,
        issued_at: f64,
    ) -> CognitiveReport {
        // `actuation` gates emitting intents (real commands); `visible` gates
        // exposing the proposed plan (tasks/tactics) for operator review.
        let (mission, pending_approval_id, denial_reason, actuation, visible) = match outcome {
            PlanningOutcome::Approved(mission) => (mission, None, None, true, true),
            PlanningOutcome::Pending {
                approval_id,
                mission,
            } => (mission, Some(approval_id), None, false, true),
            PlanningOutcome::Denied { reason, mission } => {
                (mission, None, Some(reason), false, false)
            }
        };

        let tasks = if visible {
            self.decomposer.decompose(&mission)
        } else {
            Vec::new()
        };
        let mut tactics = Vec::new();
        let mut intents = Vec::new();
        if visible {
            for task in tasks.iter().cloned() {
                if let Ok(scheduled) = self.scheduler.schedule(task, issued_at) {
                    tactics.push(scheduled.tactic);
                    if actuation {
                        intents.extend(scheduled.intents);
                    }
                }
            }
        }

        let alloc_count = mission.allocations.len();
        CognitiveReport {
            assessment,
            mission,
            tasks,
            tactics,
            intents,
            pending_approval_id,
            denial_reason,
            // Defaults assume no LLM participation; `run_once_refined` overrides
            // these when a refiner is wired in.
            llm_refine_enabled: false,
            llm_refined: false,
            baseline_allocation_count: alloc_count,
            final_allocation_count: alloc_count,
            // Populated by the slow-loop task after the workflow trigger manager
            // evaluates this cycle's assessment.
            fired_workflows: Vec::new(),
            // Populated by `run_once_refined` from the LLM policy; the kernel
            // decides whether to honor them (config + ROE gated).
            authorization_proposals: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CognitiveReport {
    pub assessment: SituationAssessment,
    pub mission: MissionConfig,
    pub tasks: Vec<Task>,
    pub tactics: Vec<Tactic>,
    /// Actuation intents — non-empty ONLY for an approved plan.
    pub intents: Vec<CandidateIntent>,
    /// Set when the plan is held at the `mission_approval` checkpoint.
    pub pending_approval_id: Option<String>,
    /// Set when the plan was denied at the checkpoint.
    pub denial_reason: Option<String>,
    /// Whether an LLM refiner was wired in for this cycle (`planning.llm_refine`).
    pub llm_refine_enabled: bool,
    /// Whether the LLM refiner actually returned a usable refinement this cycle.
    /// `false` means the deterministic rule baseline stood (LLM off, no
    /// opportunities, timeout, parse error, or empty selection).
    pub llm_refined: bool,
    /// Rule-derived engagement options before any LLM narrowing.
    pub baseline_allocation_count: usize,
    /// Engagement options after LLM narrowing (== baseline when no refine).
    pub final_allocation_count: usize,
    /// Tactical workflows the brain fired this cycle (own + formation scope).
    /// Empty when workflow orchestration is disabled.
    pub fired_workflows: Vec<crate::workflow_trigger::FiredWorkflow>,
    /// Fire-authorization proposals from the LLM policy this cycle. The kernel
    /// honors them only when config opt-in is set AND ROE is weapons-free;
    /// otherwise they are recorded as proposals for human confirmation.
    pub authorization_proposals: Vec<crate::planning::AuthorizationProposal>,
}
