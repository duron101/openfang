//! CMS — Communications Management Service.
//!
//! A first-class [`CerebellumService`] for the link-management lane. Where the
//! out-of-loop [`crate::comm_monitor`] task pings peers and the control loop's
//! autonomy-degradation stage reacts to link quality, this service makes the
//! *commanded link strategy* an explicit, testable closed loop on the hot path:
//! observe `own_platform.link.quality` → decide the matching [`LinkStrategy`] →
//! emit a `SetLinkStrategy` intent → observe convergence on the next snapshot.
//!
//! Invariants:
//! - **Idempotent**: emits `SetLinkStrategy` only when the desired strategy
//!   differs from the live one, so a stable link produces nothing.
//! - **Tolerant**: a snapshot without a `link` report (`None`) is a no-op.
//! - **Intent-only**: like every cerebellum service it never touches an adapter
//!   directly; the gate downstream remains authoritative.

use openfang_types::platform::{LinkQuality, LinkStrategy, PlatformCommand};
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};

use crate::cerebellum_services::{
    CerebellumService, CerebellumServiceId, ServiceAuditHint, ServiceContext, ServiceOutput,
};

#[derive(Debug, Default)]
pub struct CommunicationsManagementService;

impl CommunicationsManagementService {
    pub fn new() -> Self {
        Self
    }
}

impl CerebellumService for CommunicationsManagementService {
    fn id(&self) -> CerebellumServiceId {
        CerebellumServiceId::Cms
    }

    fn evaluate(&mut self, ctx: &ServiceContext<'_>) -> ServiceOutput {
        let Some(state) = ctx.own_platform else {
            return ServiceOutput::empty();
        };
        let Some(link) = state.link else {
            return ServiceOutput::empty();
        };

        let desired = desired_strategy(link.quality);
        if desired == link.strategy {
            return ServiceOutput::empty();
        }

        // A defensive shift (toward Silent/BurstOnly) is higher priority than a
        // restore toward Default once the link recovers.
        let priority = if defensiveness(desired) > defensiveness(link.strategy) {
            CommandPriority::High
        } else {
            CommandPriority::Normal
        };

        let mut out = ServiceOutput::empty();
        out.intents.push(CandidateIntent::new(
            PlatformCommand::SetLinkStrategy {
                platform_id: state.id.clone(),
                strategy: desired,
            },
            priority,
            IntentSource::Dcc {
                rule_name: format!("cms:{}", CerebellumServiceId::Cms.label()),
            },
            ctx.now,
            format!(
                "cms link {:?} → strategy {} (was {})",
                link.quality,
                desired.as_str(),
                link.strategy.as_str()
            ),
        ));
        out.audit_hints.push(
            ServiceAuditHint::new(CerebellumServiceId::Cms, "cms_link_strategy").with_detail(
                format!(
                    "quality={:?} from={} to={}",
                    link.quality,
                    link.strategy.as_str(),
                    desired.as_str()
                ),
            ),
        );
        out
    }
}

/// Map an observed link quality to the link strategy that conserves the most
/// headroom while still meeting the quality's transmission budget.
fn desired_strategy(quality: LinkQuality) -> LinkStrategy {
    match quality {
        LinkQuality::Lost => LinkStrategy::Silent,
        LinkQuality::Poor => LinkStrategy::BurstOnly,
        LinkQuality::Marginal => LinkStrategy::LowBandwidth,
        LinkQuality::Good | LinkQuality::Excellent => LinkStrategy::Default,
    }
}

/// Ordinal "how restrictive" rank used to distinguish a defensive shift from a
/// restore, so the two get different command priorities.
fn defensiveness(strategy: LinkStrategy) -> u8 {
    match strategy {
        LinkStrategy::Default => 0,
        LinkStrategy::LowBandwidth => 1,
        LinkStrategy::BurstOnly => 2,
        LinkStrategy::Silent => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::AutonomyConfig;
    use openfang_types::platform::{CcaRole, LinkStatusReport, PlatformCapabilities, PlatformState};

    fn run(link: Option<LinkStatusReport>) -> ServiceOutput {
        let caps = PlatformCapabilities::default();
        let cfg = AutonomyConfig::default();
        let active = cfg.active();
        let mut state = PlatformState::minimal("self");
        state.link = link;
        let ctx = ServiceContext {
            snapshot: None,
            own_platform: Some(&state),
            fused_tracks: &[],
            autonomy: Some(&active),
            capabilities: &caps,
            posture: CcaRole::Adaptive,
            now: 1.0,
            own_platform_id: "self",
        };
        CommunicationsManagementService::new().evaluate(&ctx)
    }

    fn link(quality: LinkQuality, strategy: LinkStrategy) -> LinkStatusReport {
        LinkStatusReport {
            quality,
            last_heartbeat_age_s: 1.0,
            strategy,
        }
    }

    #[test]
    fn no_link_report_is_noop() {
        assert!(run(None).is_empty());
    }

    #[test]
    fn stable_link_emits_nothing() {
        let out = run(Some(link(LinkQuality::Excellent, LinkStrategy::Default)));
        assert!(out.is_empty());
    }

    #[test]
    fn poor_link_commands_burst_only() {
        let out = run(Some(link(LinkQuality::Poor, LinkStrategy::Default)));
        assert_eq!(out.intents.len(), 1);
        assert!(matches!(
            out.intents[0].command,
            PlatformCommand::SetLinkStrategy { strategy: LinkStrategy::BurstOnly, .. }
        ));
        assert_eq!(out.intents[0].priority, CommandPriority::High);
    }

    #[test]
    fn recovered_link_restores_default_at_low_priority() {
        let out = run(Some(link(LinkQuality::Excellent, LinkStrategy::BurstOnly)));
        assert_eq!(out.intents.len(), 1);
        assert!(matches!(
            out.intents[0].command,
            PlatformCommand::SetLinkStrategy { strategy: LinkStrategy::Default, .. }
        ));
        assert_eq!(out.intents[0].priority, CommandPriority::Normal);
    }
}
