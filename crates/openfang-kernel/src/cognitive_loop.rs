//! Slow cognitive-loop wiring for the kernel.
//!
//! This closes the cognition → planning → decomposition → scheduling chain in
//! the *running* daemon. A background task periodically (and on freshly injected
//! commander intents) runs [`CognitivePipeline`] against the latest world
//! snapshot and injects the resulting standing plan into the fast control loop.
//!
//! ## LLM participation (hybrid, bounded)
//!
//! The pipeline is **rule-based by default**. When `platform.planning.llm_refine`
//! is enabled, [`LlmMissionPlanRefiner`] lets an LLM *re-prioritize among the
//! rule-validated engagement opportunities* — it can never invent a target, only
//! select/narrow the baseline allocations and pick a phase. Any failure
//! (timeout, transport error, parse error, missing key) falls back to the
//! deterministic baseline. Mission decomposition and playbook scheduling stay
//! fully deterministic.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use openfang_runtime::audit::{AuditAction, AuditLog};
use openfang_runtime::drivers;
use openfang_runtime::llm_driver::{CompletionRequest, DriverConfig, LlmDriver, LlmError};
use openfang_runtime::planning::{MissionPlanRefiner, PlanRefinement, RefineContext};
use openfang_types::config::{DefaultModelConfig, PlanningConfig, PlatformConfig};
use openfang_types::message::Message;
use openfang_types::umaa::{CommPlan, MissionConfig, PlatformLimits, RulesOfEngagement};

const REFINER_SYSTEM_PROMPT: &str = "\
You are a maritime tactical mission-planning assistant. You produce a bounded \
TACTICAL POLICY over PRE-VALIDATED engagement opportunities produced by a \
deterministic rules engine. You MUST NOT invent targets, weapons, or platforms; \
you may only reference the provided allocation indices. \
 Respond with ONLY a single JSON object, no prose, matching exactly:\n\
{\"selected_indices\":[<int>...],\"phase\":\"patrol|engage|track|rtb\",\"objective\":\"<short text>\",\"authorize_indices\":[<int>...],\"salvo_overrides\":[{\"index\":<int>,\"salvo_size\":<1-8>}]}\n\
Rules: selected_indices is the subset to keep (engagement target selection). \
authorize_indices is the subset of KEPT indices you recommend authorizing to \
fire, per the USV's rules of engagement and threat priority — authorization is a \
PROPOSAL that a human or ROE gate still approves. salvo_overrides set rounds per \
target by weapon-employment principle (more rounds for high-value/hard targets, \
clamped 1-8). \
COMMANDER COST POLICY: when `commander_intent.cost_policy` is present, weigh \
your target selection and ordering by those weights (w_effect favors high-effect \
targets, w_time favors fast/time-critical kills, w_survive favors low-exposure \
options, w_cost penalizes munition expenditure). Respect `time_windows`: if the \
current situation timestamp is outside every provided window, prefer to defer \
(return empty authorize_indices). If `allow_degrade` is false, do NOT keep a \
partial set that abandons higher-priority threats — keep the full validated set \
or none. If unsure, return all provided indices, phase \"engage\", empty \
authorize_indices, and no salvo_overrides.";

/// Build the LLM driver used by the slow-loop refiner.
///
/// Uses `[platform.planning]` credentials when set; otherwise falls back to
/// `[default_model]`. On failure returns `Err` so the caller can skip LLM refine
/// or reuse the kernel default driver.
pub fn build_planning_driver(
    planning: &PlanningConfig,
    default_model: &DefaultModelConfig,
) -> Result<Arc<dyn LlmDriver>, LlmError> {
    let (provider, api_key, base_url) = planning.resolved_llm_endpoint(default_model);
    drivers::create_driver(&DriverConfig {
        provider,
        api_key,
        base_url,
    })
}

/// Preferred planning driver: dedicated endpoint from config, else kernel default.
///
/// Reuses `kernel_default` when `[platform.planning]` does not override the LLM
/// endpoint, so a single TOML `[default_model]` does not spawn duplicate channels.
pub fn planning_driver_or_default(
    planning: &PlanningConfig,
    default_model: &DefaultModelConfig,
    kernel_default: Arc<dyn LlmDriver>,
) -> Arc<dyn LlmDriver> {
    if !planning.endpoint_overrides_default() {
        tracing::debug!(
            "planning LLM reuses kernel default driver (no endpoint override in [platform.planning])"
        );
        return kernel_default;
    }
    match build_planning_driver(planning, default_model) {
        Ok(driver) => driver,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "planning LLM driver init failed — using kernel default driver"
            );
            kernel_default
        }
    }
}

/// LLM-backed mission-plan refiner with a hard timeout and rule fallback.
pub struct LlmMissionPlanRefiner {
    driver: Arc<dyn LlmDriver>,
    model: String,
    timeout: Duration,
    audit: Arc<AuditLog>,
}

impl LlmMissionPlanRefiner {
    pub fn new(
        driver: Arc<dyn LlmDriver>,
        model: String,
        timeout: Duration,
        audit: Arc<AuditLog>,
    ) -> Self {
        Self {
            driver,
            model,
            timeout,
            audit,
        }
    }

    fn user_prompt(ctx: &RefineContext) -> String {
        let allocations: Vec<serde_json::Value> = ctx
            .baseline
            .allocations
            .iter()
            .enumerate()
            .map(|(idx, alloc)| {
                serde_json::json!({
                    "index": idx,
                    "platform_id": alloc.platform_id,
                    "weapon_id": alloc.weapon_id,
                    "track_id": alloc.track_id,
                })
            })
            .collect();
        // Surface the commander cost policy / timing / degradation knobs at the
        // top level (in addition to the full intent) so the model reliably
        // applies them rather than burying them inside the intent blob.
        let (cost_policy, time_windows, allow_degrade) = match &ctx.intent {
            Some(intent) => (
                serde_json::to_value(&intent.cost_policy).unwrap_or_default(),
                serde_json::to_value(&intent.time_windows).unwrap_or_default(),
                serde_json::Value::Bool(intent.allow_degrade),
            ),
            None => (
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Bool(false),
            ),
        };
        let payload = serde_json::json!({
            "summary": ctx.assessment.summary,
            "timestamp": ctx.assessment.timestamp,
            "threats": ctx.assessment.threats,
            "opportunities": ctx.assessment.opportunities,
            "own_force": ctx.assessment.own_force,
            "commander_intent": ctx.intent,
            "cost_policy": cost_policy,
            "time_windows": time_windows,
            "allow_degrade": allow_degrade,
            "baseline_allocations": allocations,
        });
        format!(
            "Situation and rule-validated baseline plan:\n{}\n\nReturn the refinement JSON.",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
        )
    }
}

#[async_trait]
impl MissionPlanRefiner for LlmMissionPlanRefiner {
    async fn refine(&self, ctx: RefineContext) -> Option<PlanRefinement> {
        // Nothing to refine when the baseline has no engagement options.
        if ctx.baseline.allocations.is_empty() {
            return None;
        }

        let request = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::user(Self::user_prompt(&ctx))],
            tools: Vec::new(),
            max_tokens: 512,
            temperature: 0.0,
            system: Some(REFINER_SYSTEM_PROMPT.to_string()),
            thinking: None,
        };

        let text = match tokio::time::timeout(self.timeout, self.driver.complete(request)).await {
            Ok(Ok(response)) => response.text(),
            Ok(Err(err)) => {
                self.audit.record(
                    "planner",
                    AuditAction::ConfigChange,
                    format!("planning LLM refine failed, using rule baseline: {err}"),
                    &ctx.baseline.mission_id,
                );
                return None;
            }
            Err(_) => {
                self.audit.record(
                    "planner",
                    AuditAction::ConfigChange,
                    "planning LLM refine timed out, using rule baseline",
                    &ctx.baseline.mission_id,
                );
                return None;
            }
        };

        match parse_refinement(&text, ctx.baseline.allocations.len()) {
            Some(refinement) => Some(refinement),
            None => {
                self.audit.record(
                    "planner",
                    AuditAction::ConfigChange,
                    "planning LLM refine returned unparseable output, using rule baseline",
                    &ctx.baseline.mission_id,
                );
                None
            }
        }
    }
}

/// Extract and validate a [`PlanRefinement`] from raw model text. Out-of-range
/// indices are dropped here as a first guard (the apply step guards again).
fn parse_refinement(text: &str, allocation_count: usize) -> Option<PlanRefinement> {
    let json = extract_json_object(text)?;
    let mut refinement: PlanRefinement = serde_json::from_str(&json).ok()?;
    refinement
        .selected_indices
        .retain(|idx| *idx < allocation_count);
    // Same first-guard for the policy fields; apply_refinement guards again and
    // clamps salvo sizes.
    refinement
        .authorize_indices
        .retain(|idx| *idx < allocation_count);
    refinement
        .salvo_overrides
        .retain(|ovr| ovr.index < allocation_count);
    Some(refinement)
}

/// Find the first balanced top-level `{...}` object in `text` (models often wrap
/// JSON in prose or code fences).
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=start + offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Build the standing base mission the slow loop re-plans from each cycle.
pub fn base_mission(cfg: &PlatformConfig) -> MissionConfig {
    MissionConfig {
        mission_id: format!("slow-loop:{}", cfg.own_platform_id),
        roe: RulesOfEngagement::default(),
        geofences: Vec::new(),
        platform_limits: PlatformLimits::default(),
        comm_plan: CommPlan::default(),
        contingency_plans: cfg.contingency_plans.clone(),
        activated_at: None,
        autonomy_mode: Default::default(),
        phase: None,
        objectives: Vec::new(),
        allocations: Vec::new(),
        target_track_id: None,
        play_name: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_object_handles_fenced_and_braced_strings() {
        let text = "Here you go:\n```json\n{\"selected_indices\":[0,1],\"phase\":\"engage\",\"objective\":\"hit {primary}\"}\n```";
        let json = extract_json_object(text).expect("object found");
        let parsed: PlanRefinement = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.selected_indices, vec![0, 1]);
        assert_eq!(parsed.phase.as_deref(), Some("engage"));
    }

    #[test]
    fn parse_refinement_drops_out_of_range_indices() {
        let text = "{\"selected_indices\":[0,5,2],\"phase\":\"engage\"}";
        let parsed = parse_refinement(text, 3).unwrap();
        assert_eq!(parsed.selected_indices, vec![0, 2]);
    }

    #[test]
    fn parse_refinement_rejects_non_json() {
        assert!(parse_refinement("no json here", 3).is_none());
    }
}
