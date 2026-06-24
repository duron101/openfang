//! Agent runtime and execution environment.
//!
//! Manages the agent execution loop, LLM driver abstraction,
//! tool execution, and WASM sandboxing for untrusted skill/plugin code.

// The runtime crate accumulates pre-existing dead-code / unused-import warnings
// from large modules (browser, web_search, etc.) that are not actively used in
// tactical builds. The tactical additions here are lint-clean; these allows
// preserve the rest of the file's surface API for the default `full` feature.
// TODO: clean up legacy warnings in a follow-up pass.
#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    unused_mut,
    clippy::empty_line_after_doc_comments,
    clippy::unnecessary_map_or
)]

pub mod a2a;
pub mod action_composer;
pub mod agent_loop;
pub mod apply_patch;
pub mod audio_dsp;
pub mod audit;
pub mod auth_cooldown;
pub mod browser;
pub mod cca_role;
pub mod cerebellum;
pub mod cerebellum_services;
pub mod cognition;
pub mod cognitive_pipeline;
pub mod colregs;
pub mod cms_service;
pub mod comm_monitor;
pub mod command_gate;
pub mod command_lane;
pub mod compactor;
pub mod context_budget;
pub mod context_overflow;
pub mod copilot_oauth;
pub mod direct_channel;
pub mod docker_sandbox;
pub mod drivers;
pub mod embedding;
pub mod engagement_guard;
pub mod ewms_service;
pub mod federation;
pub mod flank_geometry;
pub mod fleet_manager;
pub mod function_executor;
pub mod geo_zones;
pub mod graceful_shutdown;
pub mod health_monitor;
pub mod hooks;
pub mod host_functions;
pub mod image_gen;
pub mod intent_extractor;
pub mod intervention;
pub mod kernel_handle;
pub mod link_understanding;
pub mod llm_driver;
pub mod llm_errors;
pub mod loop_guard;
pub mod maneuver_service;
pub mod mcp;
pub mod mcp_server;
pub mod media_understanding;
pub mod mission_approval;
pub mod mission_compiler;
pub mod mission_config;
pub mod mission_registry;
pub mod mission_scheduler;
pub mod model_catalog;
pub mod nav_control;
pub mod navigation_service;
pub mod op_restrictions;
pub mod planning;
pub mod platform_allocator;
pub mod platform_tools;
pub mod play_registry;
pub mod playbook_scheduler;
pub mod process_manager;
pub mod prompt_builder;
pub mod provider_health;
pub mod python_runtime;
pub mod reply_directives;
pub mod report_queue;
pub mod retry;
pub mod route_geometry;
pub mod route_planner;
pub mod routing;
pub mod sandbox;
pub mod sensor_fusion;
pub mod sensor_management;
pub mod sensor_policy;
pub mod session_repair;
pub mod shell_bleed;
pub mod str_utils;
pub mod subprocess_sandbox;
pub mod survivability_service;
pub mod tactical_policy;
pub mod target_authorization;
pub mod task_execution;
pub mod tool_policy;
pub mod tool_runner;
pub mod track_manager;
pub mod tts;
pub mod weapon_engagement;
pub mod weapon_interface;
pub mod web_cache;
pub mod web_content;
pub mod web_fetch;
pub mod web_search;
pub mod wms_policy;
pub mod workflow_trigger;
pub mod workspace_context;
pub mod workspace_sandbox;

#[cfg(test)]
mod cognition_loop_tests {
    use openfang_types::platform::{
        Affiliation, Domain, FuelStatus, PlatformState, Pose, Track, Velocity, WeaponState,
        WorldSnapshot,
    };

    #[test]
    fn cognition_engine_extracts_threats_opportunities_and_own_force() {
        let own = PlatformState {
            id: "usv-01".into(),
            name: "usv-01".into(),
            platform_type: "usv".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Surface,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 0.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 10.0,
                vertical_rate_ms: 0.0,
                course_deg: 0.0,
            },
            fuel: FuelStatus {
                remaining_kg: 50.0,
                max_kg: 100.0,
                consumption_rate_kg_s: 0.1,
            },
            damage: 0.2,
            tracks: vec![Track {
                track_id: "trk-1".into(),
                target_name: String::new(),
                classification: "usv".into(),
                affiliation: Affiliation::Red,
                iff: "foe".into(),
                position_lla: None,
                heading_deg: None,
                speed_ms: Some(20.0),
                range_m: Some(1_000.0),
                bearing_deg: Some(90.0),
                elevation_deg: None,
                quality: 0.9,
                stale: false,
                last_update_s: 12.0,
                is_active: true,
            }],
            onboard_sensors: vec![],
            onboard_weapons: vec![WeaponState {
                weapon_id: "gun".into(),
                weapon_type: "cannon".into(),
                quantity_remaining: 10.0,
                max_range_m: Some(2_000.0),
                min_range_m: Some(0.0),
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            }],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        };
        let snapshot = WorldSnapshot {
            timestamp: 12.0,
            platforms: vec![own],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        let assessment = crate::cognition::CognitionEngine::default().assess(&snapshot);

        assert_eq!(assessment.timestamp, 12.0);
        assert_eq!(assessment.threats[0].track_id, "trk-1");
        assert_eq!(assessment.opportunities[0].weapon_id, "gun");
        assert_eq!(assessment.own_force.total_platforms, 1);
        assert_eq!(assessment.own_force.average_fuel_pct, 0.5);
    }
}

#[cfg(test)]
mod planning_tests {
    use std::sync::Arc;

    use openfang_types::cognition::{
        CommanderIntent, EngageOpportunity, OwnForceStatus, SituationAssessment, ThreatTrack,
    };
    use openfang_types::umaa::{
        AutonomyMode, CommPlan, MissionConfig, Objective, PlatformLimits, RulesOfEngagement,
    };

    fn base_mission() -> MissionConfig {
        MissionConfig {
            mission_id: "m1".into(),
            roe: RulesOfEngagement::default(),
            geofences: vec![],
            platform_limits: PlatformLimits::default(),
            comm_plan: CommPlan::default(),
            contingency_plans: vec![],
            activated_at: None,
            autonomy_mode: AutonomyMode::HumanSupervised,
            phase: None,
            objectives: vec![],
            allocations: vec![],
            target_track_id: None,
            play_name: None,
        }
    }

    fn assessment() -> SituationAssessment {
        SituationAssessment {
            timestamp: 1.0,
            threats: vec![ThreatTrack {
                track_id: "trk-1".into(),
                platform_type: "usv".into(),
                distance_m: 1_000.0,
                closing_rate_ms: 10.0,
                threat_score: 0.8,
            }],
            opportunities: vec![EngageOpportunity {
                platform_id: "usv-01".into(),
                weapon_id: "gun".into(),
                track_id: "trk-1".into(),
                estimated_p_hit: 0.7,
            }],
            own_force: OwnForceStatus {
                total_platforms: 1,
                average_damage: 0.0,
                average_fuel_pct: 0.8,
                link_status: "connected".into(),
            },
            summary: "threat".into(),
        }
    }

    #[test]
    fn intent_inbox_pops_commander_intents_fifo() {
        let inbox = crate::planning::IntentInbox::new();
        inbox.submit(CommanderIntent {
            id: "i1".into(),
            issued_at: 1.0,
            issued_by: "operator".into(),
            objective: "engage hostile".into(),
            priority_tracks: vec!["trk-1".into()],
            priority_labels: vec![],
            constraints: vec![],
            roe_pref: None,
            cost_policy: Default::default(),
            time_windows: vec![],
            allow_degrade: false,
        });

        assert_eq!(inbox.pop_next().unwrap().id, "i1");
        assert!(inbox.pop_next().is_none());
    }

    #[test]
    fn intent_inbox_peek_and_ack_preserve_pending_front() {
        let inbox = crate::planning::IntentInbox::new();
        inbox.submit(CommanderIntent {
            id: "i1".into(),
            issued_at: 1.0,
            issued_by: "operator".into(),
            objective: "engage hostile".into(),
            priority_tracks: vec!["trk-1".into()],
            priority_labels: vec![],
            constraints: vec![],
            roe_pref: None,
            cost_policy: Default::default(),
            time_windows: vec![],
            allow_degrade: false,
        });

        assert_eq!(inbox.peek_next().unwrap().id, "i1");
        assert_eq!(inbox.peek_next().unwrap().id, "i1");
        assert_eq!(inbox.len(), 1, "peek must not consume pending intents");
        assert!(inbox.ack_next("wrong-id").is_none());
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox.ack_next("i1").unwrap().id, "i1");
        assert!(inbox.is_empty());
    }

    #[test]
    fn planner_updates_mission_with_objective_and_allocation() {
        let planner = crate::planning::Planner::new(Arc::new(std::sync::RwLock::new(
            crate::intervention::InterventionGate::new(
                Default::default(),
                Arc::new(crate::target_authorization::TargetAuthorizationRegistry::new()),
                Arc::new(crate::mission_approval::MissionApprovalRegistry::new()),
            ),
        )));

        let outcome = planner.plan(&assessment(), None, base_mission());
        let mission = outcome.approved().unwrap();

        assert_eq!(mission.phase.as_deref(), Some("engage"));
        assert_eq!(mission.objectives.len(), 1);
        assert_eq!(mission.allocations[0].track_id, "trk-1");
    }

    #[test]
    fn plan_fingerprint_changes_when_visible_objective_changes() {
        let mut mission = base_mission();
        mission.phase = Some("engage".into());
        mission.objectives = vec![Objective {
            id: "obj-1".into(),
            description: "hold fire".into(),
            priority: 10,
            status: "pending".into(),
        }];
        let first = crate::planning::plan_fingerprint(&mission);

        mission.objectives[0].description = "engage hostile".into();
        let second = crate::planning::plan_fingerprint(&mission);

        assert_ne!(first, second);
    }

    #[test]
    fn refinement_can_only_narrow_baseline_allocations() {
        use crate::planning::{apply_refinement, PlanRefinement};
        use openfang_types::umaa::TargetAllocation;

        let mut mission = base_mission();
        mission.allocations = vec![
            TargetAllocation {
                platform_id: "self".into(),
                weapon_id: "w1".into(),
                track_id: "trk-1".into(),
                allocated_at: 0.0,
                ..Default::default()
            },
            TargetAllocation {
                platform_id: "self".into(),
                weapon_id: "w2".into(),
                track_id: "trk-2".into(),
                allocated_at: 0.0,
                ..Default::default()
            },
        ];
        // Out-of-range indices are ignored; only index 1 survives.
        let refined = apply_refinement(
            mission,
            &PlanRefinement {
                selected_indices: vec![1, 99],
                phase: Some("engage".into()),
                objective: None,
                ..Default::default()
            },
        );
        assert_eq!(refined.allocations.len(), 1);
        assert_eq!(refined.allocations[0].track_id, "trk-2");
    }
}

#[cfg(test)]
mod playbook_scheduler_tests {
    use openfang_types::cognition::TaskKind;
    use openfang_types::platform::PlatformCommand;
    use openfang_types::tactical::CommandPriority;
    use openfang_types::umaa::{
        AutonomyMode, CommPlan, MissionConfig, PlatformLimits, RulesOfEngagement, TargetAllocation,
    };

    fn mission_with_allocation() -> MissionConfig {
        MissionConfig {
            mission_id: "m1".into(),
            roe: RulesOfEngagement::default(),
            geofences: vec![],
            platform_limits: PlatformLimits::default(),
            comm_plan: CommPlan::default(),
            contingency_plans: vec![],
            activated_at: None,
            autonomy_mode: AutonomyMode::HumanSupervised,
            phase: Some("engage".into()),
            objectives: vec![],
            allocations: vec![TargetAllocation {
                platform_id: "usv-01".into(),
                weapon_id: "gun".into(),
                track_id: "trk-1".into(),
                allocated_at: 1.0,
                ..Default::default()
            }],
            target_track_id: None,
            play_name: None,
        }
    }

    #[test]
    fn decomposer_turns_allocations_into_engage_tasks() {
        let tasks = crate::playbook_scheduler::MissionDecomposer::new()
            .decompose(&mission_with_allocation());

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, TaskKind::Engage);
        assert_eq!(tasks[0].assignee, "usv-01");
        assert_eq!(tasks[0].priority, CommandPriority::High);
    }

    #[test]
    fn scheduler_turns_engage_task_into_fire_intent() {
        let task = crate::playbook_scheduler::MissionDecomposer::new()
            .decompose(&mission_with_allocation())
            .remove(0);
        let scheduled = crate::playbook_scheduler::PlaybookScheduler::new()
            .schedule(task, 2.0)
            .unwrap();

        assert_eq!(scheduled.tactic.playbook, "Engage");
        assert_eq!(scheduled.intents.len(), 1);
        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::FireAtTarget { .. }
        ));
    }

    #[test]
    fn scheduler_emits_fire_salvo_when_brain_sets_salvo_size() {
        let mut mission = mission_with_allocation();
        mission.allocations[0].salvo_size = Some(3);
        let task = crate::playbook_scheduler::MissionDecomposer::new()
            .decompose(&mission)
            .remove(0);
        let scheduled = crate::playbook_scheduler::PlaybookScheduler::new()
            .schedule(task, 2.0)
            .unwrap();

        assert_eq!(scheduled.intents.len(), 1);
        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::FireSalvo { salvo_size: 3, .. }
        ));
    }

    #[test]
    fn scheduler_keeps_single_fire_when_salvo_size_is_one() {
        let mut mission = mission_with_allocation();
        mission.allocations[0].salvo_size = Some(1);
        let task = crate::playbook_scheduler::MissionDecomposer::new()
            .decompose(&mission)
            .remove(0);
        let scheduled = crate::playbook_scheduler::PlaybookScheduler::new()
            .schedule(task, 2.0)
            .unwrap();

        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::FireAtTarget { .. }
        ));
    }
}

#[cfg(test)]
mod cognitive_pipeline_tests {
    use std::sync::Arc;

    use openfang_types::platform::{
        Affiliation, Domain, FuelStatus, PlatformState, Pose, Track, Velocity, WeaponState,
        WorldSnapshot,
    };
    use openfang_types::umaa::{
        AutonomyMode, CommPlan, MissionConfig, PlatformLimits, RulesOfEngagement,
    };

    fn base_mission() -> MissionConfig {
        MissionConfig {
            mission_id: "m1".into(),
            roe: RulesOfEngagement::default(),
            geofences: vec![],
            platform_limits: PlatformLimits::default(),
            comm_plan: CommPlan::default(),
            contingency_plans: vec![],
            activated_at: None,
            autonomy_mode: AutonomyMode::HumanSupervised,
            phase: None,
            objectives: vec![],
            allocations: vec![],
            target_track_id: None,
            play_name: None,
        }
    }

    fn snapshot() -> WorldSnapshot {
        let mut own = PlatformState::minimal("usv-01");
        own.affiliation = Affiliation::Blue;
        own.domain = Domain::Surface;
        own.pose = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        own.velocity = Velocity {
            speed_ms: 10.0,
            vertical_rate_ms: 0.0,
            course_deg: 0.0,
        };
        own.fuel = FuelStatus {
            remaining_kg: 80.0,
            max_kg: 100.0,
            consumption_rate_kg_s: 0.1,
        };
        own.tracks = vec![Track {
            track_id: "trk-1".into(),
            target_name: String::new(),
            classification: "usv".into(),
            affiliation: Affiliation::Red,
            iff: "foe".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: Some(15.0),
            range_m: Some(1_000.0),
            bearing_deg: Some(90.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        }];
        own.onboard_weapons = vec![WeaponState {
            weapon_id: "gun".into(),
            weapon_type: "cannon".into(),
            quantity_remaining: 10.0,
            max_range_m: Some(2_000.0),
            min_range_m: Some(0.0),
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        }];
        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![own],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn gate(
        config: openfang_types::config::InterventionConfig,
    ) -> Arc<std::sync::RwLock<crate::intervention::InterventionGate>> {
        Arc::new(std::sync::RwLock::new(
            crate::intervention::InterventionGate::new(
                config,
                Arc::new(crate::target_authorization::TargetAuthorizationRegistry::new()),
                Arc::new(crate::mission_approval::MissionApprovalRegistry::new()),
            ),
        ))
    }

    #[test]
    fn cognitive_pipeline_produces_intents_from_snapshot() {
        let pipeline = crate::cognitive_pipeline::CognitivePipeline::new(gate(Default::default()));

        let report = pipeline.run_once(&snapshot(), base_mission());

        assert_eq!(report.assessment.threats.len(), 1);
        assert_eq!(report.mission.allocations.len(), 1);
        assert_eq!(report.tasks.len(), 1);
        assert_eq!(report.intents.len(), 1);
    }

    #[test]
    fn pending_mission_withholds_actuation_intents() {
        use openfang_types::config::{InterventionConfig, InterventionMode};

        // Force the mission_approval checkpoint to Pending (Confirm mode).
        let pipeline =
            crate::cognitive_pipeline::CognitivePipeline::new(gate(InterventionConfig {
                default_mode: InterventionMode::Confirm,
                rules: vec![],
            }));

        let report = pipeline.run_once(&snapshot(), base_mission());

        assert!(
            report.pending_approval_id.is_some(),
            "should be held pending"
        );
        // Tasks/tactics are visible for operator review, but NO actuation intents
        // may be emitted until the plan is approved.
        assert_eq!(report.tasks.len(), 1);
        assert!(report.intents.is_empty(), "pending plan must not actuate");
    }

    #[test]
    fn denied_mission_emits_nothing() {
        use openfang_types::config::{InterventionConfig, InterventionMode};

        let pipeline =
            crate::cognitive_pipeline::CognitivePipeline::new(gate(InterventionConfig {
                default_mode: InterventionMode::Deny,
                rules: vec![],
            }));

        let report = pipeline.run_once(&snapshot(), base_mission());

        assert!(report.denial_reason.is_some());
        assert!(report.tasks.is_empty());
        assert!(report.tactics.is_empty());
        assert!(report.intents.is_empty());
    }

    #[test]
    fn confirm_mode_releases_plan_after_fingerprint_approval() {
        use openfang_types::config::{InterventionConfig, InterventionMode};

        let targets = Arc::new(crate::target_authorization::TargetAuthorizationRegistry::new());
        let approvals = Arc::new(crate::mission_approval::MissionApprovalRegistry::new());
        let shared_gate = Arc::new(std::sync::RwLock::new(
            crate::intervention::InterventionGate::new(
                InterventionConfig {
                    default_mode: InterventionMode::Confirm,
                    rules: vec![],
                },
                Arc::clone(&targets),
                Arc::clone(&approvals),
            ),
        ));
        let pipeline = crate::cognitive_pipeline::CognitivePipeline::new(Arc::clone(&shared_gate));

        // First cycle: plan is held pending under its fingerprint.
        let held = pipeline.run_once(&snapshot(), base_mission());
        let fingerprint = held.pending_approval_id.clone().expect("plan held pending");
        assert!(held.intents.is_empty());

        // Operator approves that exact fingerprint.
        approvals.approve(&fingerprint, "operator", 1.0);

        // Next cycle with identical situation → same fingerprint → released.
        let released = pipeline.run_once(&snapshot(), base_mission());
        assert!(
            released.pending_approval_id.is_none(),
            "approved plan released"
        );
        assert_eq!(released.intents.len(), 1, "approved plan actuates");
    }

    #[tokio::test]
    async fn refiner_narrows_allocations_before_gate() {
        use crate::planning::{MissionPlanRefiner, PlanRefinement, RefineContext};

        // A snapshot with two reachable threats → two baseline allocations.
        let mut snap = snapshot();
        let mut t2 = snap.platforms[0].tracks[0].clone();
        t2.track_id = "trk-2".into();
        snap.platforms[0].tracks.push(t2);

        struct PickFirst;
        #[async_trait::async_trait]
        impl MissionPlanRefiner for PickFirst {
            async fn refine(&self, ctx: RefineContext) -> Option<PlanRefinement> {
                assert_eq!(ctx.baseline.allocations.len(), 2, "baseline has both");
                Some(PlanRefinement {
                    selected_indices: vec![0],
                    phase: None,
                    objective: Some("focus highest-value target".into()),
                    ..Default::default()
                })
            }
        }

        let pipeline = crate::cognitive_pipeline::CognitivePipeline::new(gate(Default::default()));
        let refiner = PickFirst;
        let report = pipeline
            .run_once_refined(&snap, base_mission(), None, Some(&refiner))
            .await;

        assert_eq!(
            report.mission.allocations.len(),
            1,
            "refiner narrowed to one"
        );
        assert_eq!(report.intents.len(), 1);
        assert_eq!(
            report.mission.objectives[0].description,
            "focus highest-value target"
        );
    }
}
