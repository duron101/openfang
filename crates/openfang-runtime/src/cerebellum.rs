//! Minimal cerebellum tick scheduler — the real-time boundary.
//!
//! Phase 4 lands only the four mandatory boundaries the closed loop needs:
//! - **SMS** (state): consume the latest snapshot (already normalized upstream),
//! - **ACS** (action composition): deconflict slow + fast loop intents,
//! - **SPGS** (safety pre-screen): cheap ROE / limit / geofence rejection,
//! - **MMS** (maneuver): emit the surviving intents for dispatch.
//!
//! Real-time discipline:
//! - Inputs arrive on a **bounded** queue that coalesces by effector and drops
//!   oldest on overflow (no unbounded `Mutex<Vec<…>>` growth).
//! - The hot path performs **no network I/O and no blocking approval** — weapon
//!   authorization is handled asynchronously elsewhere.
//! - Each tick is measured against a **budget**; missed deadlines and per-stage
//!   timings are recorded as telemetry.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use openfang_types::platform::{PlatformCapabilities, PlatformState, WorldSnapshot};
use openfang_types::tactical::{CandidateIntent, CommandClass, CommandPriority, IntentSource};

use crate::action_composer::ActionComposer;
use crate::cca_role::{posture_commands, RolePosture};
use crate::op_restrictions::OpRestrictionsManager;

// ─────────────────────────────────────────────
// Domain reflex lanes ("若干小脑")
// ─────────────────────────────────────────────

/// A domain reflex lane. The cerebellum is partitioned into independent
/// subsystem lanes, each owning one slice of the [`RolePosture`] and gated by
/// the platform's [`PlatformCapabilities`]. A lane with no underlying hardware
/// (e.g. EW on a platform with no jammer) is inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneKind {
    /// Motion / collision / geofence / formation geometry (`Motion`,`Uav`,`Formation`).
    Nav,
    /// Active-emitter sensors (radar) under EMCON; cueing reflexes (`Sensor`).
    Sensor,
    /// Electronic attack — jammers (`ElectronicWarfare`).
    Ew,
    /// Weapon safing — the Iron Law. Only ever *safes*; never arms or fires (`Weapon`).
    WeaponSafe,
    /// Datalink / comms emission under EMCON (`Comm`).
    Comm,
    /// Cross-cutting survival reflexes (chaff, RTB). Always on; not posture-bound.
    Survival,
}

impl LaneKind {
    /// All lanes in deterministic order.
    pub const ALL: [LaneKind; 6] = [
        LaneKind::Nav,
        LaneKind::Sensor,
        LaneKind::Ew,
        LaneKind::WeaponSafe,
        LaneKind::Comm,
        LaneKind::Survival,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            LaneKind::Nav => "nav",
            LaneKind::Sensor => "sensor",
            LaneKind::Ew => "ew",
            LaneKind::WeaponSafe => "weapon_safe",
            LaneKind::Comm => "comm",
            LaneKind::Survival => "survival",
        }
    }

    /// The lane that owns a given command class. `Command`/`Aux` are not domain
    /// reflexes and return `None`.
    pub fn for_class(class: CommandClass) -> Option<LaneKind> {
        match class {
            CommandClass::Motion | CommandClass::Uav | CommandClass::Formation => {
                Some(LaneKind::Nav)
            }
            CommandClass::Sensor => Some(LaneKind::Sensor),
            CommandClass::ElectronicWarfare => Some(LaneKind::Ew),
            CommandClass::Weapon => Some(LaneKind::WeaponSafe),
            CommandClass::Comm => Some(LaneKind::Comm),
            CommandClass::Command | CommandClass::Aux => None,
        }
    }

    /// Whether the platform's capabilities enable this lane. The `Survival` lane
    /// (chaff/RTB safety reflexes) and `WeaponSafe` lane (safing only) are always
    /// enabled — safety must not depend on a capability flag.
    pub fn enabled_by(&self, caps: &PlatformCapabilities) -> bool {
        match self {
            LaneKind::Nav => caps.supports_motion_control,
            LaneKind::Sensor => caps.supports_sensor_control,
            LaneKind::Ew => caps.supports_jammer_control,
            LaneKind::Comm => caps.supports_comm_control,
            LaneKind::WeaponSafe | LaneKind::Survival => true,
        }
    }
}

/// Per-lane status snapshot for telemetry / UI.
#[derive(Debug, Clone)]
pub struct LaneStatus {
    pub kind: LaneKind,
    /// Whether the platform capabilities enable this lane.
    pub enabled: bool,
    /// Posture-enforcement intents this lane emitted (cumulative).
    pub posture_emitted: u64,
    /// Posture-enforcement intents dropped because the lane is capability-gated.
    pub posture_gated: u64,
    /// SPGS pre-screen rejections attributed to this lane (cumulative).
    pub spgs_rejections: u64,
}

/// Index of a lane in the parallel counter arrays (parallel to [`LaneKind::ALL`]).
fn lane_index(kind: LaneKind) -> usize {
    match kind {
        LaneKind::Nav => 0,
        LaneKind::Sensor => 1,
        LaneKind::Ew => 2,
        LaneKind::WeaponSafe => 3,
        LaneKind::Comm => 4,
        LaneKind::Survival => 5,
    }
}

/// Fully-permissive capabilities — the default before an adapter reports its
/// real bitmap, so a freshly-constructed cerebellum preserves prior behavior.
fn all_caps() -> PlatformCapabilities {
    PlatformCapabilities {
        supports_motion_control: true,
        supports_sensor_control: true,
        supports_weapon_control: true,
        supports_jammer_control: true,
        supports_comm_control: true,
        supports_uav_launch_recovery: true,
        supports_formation_control: true,
        supports_handoff: true,
        max_platforms: 0,
        supports_simulation: true,
        supports_hardware: true,
    }
}

// ─────────────────────────────────────────────
// Bounded, coalescing intent queue
// ─────────────────────────────────────────────

/// Fixed-capacity queue of pending intents. New intents coalesce with an
/// existing intent for the same effector (keeping the newer/higher-priority
/// one). On genuine overflow the oldest entry is dropped and counted.
pub struct BoundedIntentQueue {
    cap: usize,
    buf: VecDeque<CandidateIntent>,
    dropped: u64,
}

impl BoundedIntentQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            buf: VecDeque::with_capacity(cap.max(1)),
            dropped: 0,
        }
    }

    /// Enqueue an intent. Never blocks, never grows beyond capacity.
    pub fn push(&mut self, intent: CandidateIntent) {
        // Coalesce: replace any existing intent for the same effector.
        if let Some(slot) = self
            .buf
            .iter_mut()
            .find(|e| e.conflict_key() == intent.conflict_key())
        {
            *slot = intent;
            return;
        }
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
            self.dropped += 1;
        }
        self.buf.push_back(intent);
    }

    /// Remove and return all queued intents.
    pub fn drain(&mut self) -> Vec<CandidateIntent> {
        self.buf.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

// ─────────────────────────────────────────────
// Telemetry
// ─────────────────────────────────────────────

/// Rolling telemetry for the cerebellum tick loop.
#[derive(Debug, Default, Clone)]
pub struct TickTelemetry {
    pub ticks: u64,
    pub deadline_misses: u64,
    pub last_tick_us: u64,
    pub max_tick_us: u64,
    pub last_compose_us: u64,
    pub last_spgs_us: u64,
    pub dropped_intents: u64,
    pub spgs_rejections: u64,
}

impl TickTelemetry {
    pub fn miss_rate(&self) -> f64 {
        if self.ticks == 0 {
            0.0
        } else {
            self.deadline_misses as f64 / self.ticks as f64
        }
    }
}

/// Output of a single tick.
#[derive(Debug, Default, Clone)]
pub struct TickOutput {
    /// Intents that survived composition + SPGS pre-screen, ready for the gate.
    pub intents: Vec<CandidateIntent>,
    /// Whether this tick exceeded its budget.
    pub deadline_missed: bool,
    /// Microseconds the tick took.
    pub tick_us: u64,
}

// ─────────────────────────────────────────────
// Cerebellum
// ─────────────────────────────────────────────

/// The minimal real-time tick scheduler, partitioned into domain reflex lanes.
pub struct Cerebellum {
    queue: BoundedIntentQueue,
    composer: ActionComposer,
    restrictions: Arc<OpRestrictionsManager>,
    budget_us: u64,
    telem: TickTelemetry,
    /// Platform capability bitmap; gates which lanes are live.
    caps: PlatformCapabilities,
    /// Latest role posture fanned out by the brain (`None` until first set).
    posture: Option<RolePosture>,
    /// Per-lane cumulative counters, indexed parallel to [`LaneKind::ALL`].
    lane_posture_emitted: [u64; 6],
    lane_posture_gated: [u64; 6],
    lane_spgs_rejections: [u64; 6],
}

impl Cerebellum {
    /// `tick_hz` sets the budget (e.g. 20Hz → 50ms). `queue_cap` bounds memory.
    pub fn new(tick_hz: f64, queue_cap: usize, restrictions: Arc<OpRestrictionsManager>) -> Self {
        let budget_us = if tick_hz > 0.0 {
            (1_000_000.0 / tick_hz) as u64
        } else {
            50_000
        };
        Self {
            queue: BoundedIntentQueue::new(queue_cap),
            composer: ActionComposer::new(),
            restrictions,
            budget_us,
            telem: TickTelemetry::default(),
            // Default-permissive: until capabilities are set, lanes follow posture
            // (so existing single-platform behavior is preserved).
            caps: all_caps(),
            posture: None,
            lane_posture_emitted: [0; 6],
            lane_posture_gated: [0; 6],
            lane_spgs_rejections: [0; 6],
        }
    }

    /// Per-tick budget in microseconds.
    pub fn budget_us(&self) -> u64 {
        self.budget_us
    }

    /// Set the platform capability bitmap that gates the domain lanes.
    pub fn set_capabilities(&mut self, caps: PlatformCapabilities) {
        self.caps = caps;
    }

    /// Fan out a new role posture to the lanes (brain → cerebellum contract).
    pub fn set_posture(&mut self, posture: RolePosture) {
        self.posture = Some(posture);
    }

    /// The currently-active posture, if any.
    pub fn posture(&self) -> Option<&RolePosture> {
        self.posture.as_ref()
    }

    /// Build the posture-enforcement intents for `self_state` under the current
    /// posture, routing each command to its domain lane and dropping commands
    /// whose lane is capability-gated off. Returns the surviving intents and
    /// updates per-lane emit/gate counters. Idempotent: [`posture_commands`]
    /// only emits where the live state diverges from the desired posture.
    pub fn posture_intents(
        &mut self,
        self_state: &PlatformState,
        now: f64,
    ) -> Vec<CandidateIntent> {
        let Some(posture) = self.posture.clone() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for cmd in posture_commands(self_state, &posture) {
            let lane = LaneKind::for_class(cmd.command_class()).unwrap_or(LaneKind::Nav);
            let idx = lane_index(lane);
            if !lane.enabled_by(&self.caps) {
                self.lane_posture_gated[idx] += 1;
                continue;
            }
            self.lane_posture_emitted[idx] += 1;
            out.push(CandidateIntent::new(
                cmd,
                CommandPriority::Normal,
                IntentSource::Llm {
                    agent_id: "cca_role".into(),
                },
                now,
                format!("posture[{}]: {:?}", lane.label(), posture.role),
            ));
        }
        out
    }

    /// Submit the current posture-enforcement intents into the bounded queue.
    pub fn enforce_posture(&mut self, self_state: &PlatformState, now: f64) {
        for intent in self.posture_intents(self_state, now) {
            self.queue.push(intent);
        }
    }

    /// Submit an intent to the bounded queue (non-blocking).
    pub fn submit(&mut self, intent: CandidateIntent) {
        self.queue.push(intent);
    }

    /// Replace the standing slow-loop plan.
    pub fn set_active_plan(&mut self, intents: Vec<CandidateIntent>) {
        self.composer.set_active_plan(intents);
    }

    /// Per-lane status snapshot for telemetry / UI.
    pub fn lane_statuses(&self) -> Vec<LaneStatus> {
        LaneKind::ALL
            .iter()
            .map(|&kind| {
                let i = lane_index(kind);
                LaneStatus {
                    kind,
                    enabled: kind.enabled_by(&self.caps),
                    posture_emitted: self.lane_posture_emitted[i],
                    posture_gated: self.lane_posture_gated[i],
                    spgs_rejections: self.lane_spgs_rejections[i],
                }
            })
            .collect()
    }

    /// Telemetry snapshot.
    pub fn telemetry(&self) -> TickTelemetry {
        let mut t = self.telem.clone();
        t.dropped_intents = self.queue.dropped();
        t
    }

    /// Run one tick: SMS (snapshot) → ACS (compose) → SPGS (pre-screen) → MMS.
    /// `_snapshot` is the read-only state for this tick (already normalized).
    pub fn tick(&mut self, _snapshot: Option<&WorldSnapshot>) -> TickOutput {
        let t0 = Instant::now();

        let new_intents = self.queue.drain();

        // ACS — composition / deconfliction.
        let c0 = Instant::now();
        let composed = self.composer.compose(new_intents);
        let compose_us = c0.elapsed().as_micros() as u64;

        // SPGS — cheap safety pre-screen (no I/O). Final authority is the gate.
        let s0 = Instant::now();
        let mut survivors = Vec::with_capacity(composed.len());
        let mut rejected = 0u64;
        for intent in composed {
            if self.spgs_prescreen(&intent) {
                survivors.push(intent);
            } else {
                rejected += 1;
                if let Some(lane) = LaneKind::for_class(intent.class()) {
                    self.lane_spgs_rejections[lane_index(lane)] += 1;
                }
            }
        }
        let spgs_us = s0.elapsed().as_micros() as u64;

        let tick_us = t0.elapsed().as_micros() as u64;
        let deadline_missed = tick_us > self.budget_us;

        // Update telemetry.
        self.telem.ticks += 1;
        self.telem.last_tick_us = tick_us;
        self.telem.max_tick_us = self.telem.max_tick_us.max(tick_us);
        self.telem.last_compose_us = compose_us;
        self.telem.last_spgs_us = spgs_us;
        self.telem.spgs_rejections += rejected;
        if deadline_missed {
            self.telem.deadline_misses += 1;
        }

        TickOutput {
            intents: survivors,
            deadline_missed,
            tick_us,
        }
    }

    /// Cheap, allocation-free SPGS pre-screen. Returns true if the intent may
    /// proceed to the authoritative gate.
    fn spgs_prescreen(&self, intent: &CandidateIntent) -> bool {
        use openfang_types::platform::PlatformCommand as P;
        // Motion limit screen.
        let speed = match &intent.command {
            P::SetSpeed { speed_ms, .. } => Some(*speed_ms),
            P::SetHeading { speed_ms, .. } => *speed_ms,
            P::GotoLocation { speed_ms, .. } => *speed_ms,
            _ => None,
        };
        if let Some(s) = speed {
            if self.restrictions.check_limits(s, 0.0).is_err() {
                return false;
            }
        }
        // Geofence screen for movement classes.
        if matches!(
            intent.class(),
            CommandClass::Motion | CommandClass::Uav | CommandClass::Formation
        ) && self.restrictions.check_geofence_violation().is_some()
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::PlatformCommand;
    use openfang_types::tactical::{CommandPriority, IntentSource};
    use openfang_types::umaa::{PlatformLimits, RulesOfEngagement};

    fn heading(p: &str, hdg: f64, t: f64) -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::SetHeading {
                platform_id: p.into(),
                heading_deg: hdg,
                speed_ms: None,
                turn_direction: None,
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "na".into(),
            },
            t,
            "nav",
        )
    }

    fn restrictions() -> Arc<OpRestrictionsManager> {
        Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement::default(),
            PlatformLimits::default(),
        ))
    }

    #[test]
    fn bounded_queue_coalesces_same_effector() {
        let mut q = BoundedIntentQueue::new(8);
        q.push(heading("usv-01", 90.0, 1.0));
        q.push(heading("usv-01", 180.0, 2.0)); // same effector → coalesce
        assert_eq!(q.len(), 1);
        let drained = q.drain();
        match &drained[0].command {
            PlatformCommand::SetHeading { heading_deg, .. } => assert_eq!(*heading_deg, 180.0),
            _ => panic!(),
        }
    }

    #[test]
    fn bounded_queue_drops_oldest_on_overflow() {
        let mut q = BoundedIntentQueue::new(2);
        q.push(heading("usv-01", 1.0, 1.0));
        q.push(heading("usv-02", 2.0, 1.0));
        q.push(heading("usv-03", 3.0, 1.0)); // overflow → drop oldest
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 1);
    }

    #[test]
    fn tick_records_telemetry_and_meets_budget() {
        let mut cer = Cerebellum::new(20.0, 256, restrictions());
        assert_eq!(cer.budget_us(), 50_000);
        for i in 0..100 {
            cer.submit(heading(
                &format!("usv-{i:03}"),
                (i as f64) % 360.0,
                i as f64,
            ));
        }
        let out = cer.tick(None);
        assert_eq!(out.intents.len(), 100);
        assert!(
            !out.deadline_missed,
            "tick took {}us > 50ms budget",
            out.tick_us
        );
        let t = cer.telemetry();
        assert_eq!(t.ticks, 1);
        assert_eq!(t.deadline_misses, 0);
    }

    #[test]
    fn spgs_prescreen_rejects_over_speed() {
        let mut cer = Cerebellum::new(20.0, 64, restrictions());
        cer.submit(CandidateIntent::new(
            PlatformCommand::SetSpeed {
                platform_id: "usv-01".into(),
                speed_ms: 999.0,
                acceleration_ms2: None,
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "na".into(),
            },
            0.0,
            "dash",
        ));
        let out = cer.tick(None);
        assert!(out.intents.is_empty());
        assert_eq!(cer.telemetry().spgs_rejections, 1);
    }

    #[test]
    fn many_ticks_stay_within_budget() {
        let mut cer = Cerebellum::new(20.0, 512, restrictions());
        for tick in 0..200 {
            for i in 0..50 {
                cer.submit(heading(
                    &format!("p-{i:02}"),
                    (i as f64) % 360.0,
                    tick as f64,
                ));
            }
            let _ = cer.tick(None);
        }
        let t = cer.telemetry();
        assert_eq!(t.ticks, 200);
        // Under pure-CPU load the 50ms budget should never be exceeded.
        assert_eq!(t.deadline_misses, 0, "max tick {}us", t.max_tick_us);
    }

    // ── Domain reflex lanes ──

    fn no_jammer_caps() -> PlatformCapabilities {
        PlatformCapabilities {
            supports_motion_control: true,
            supports_sensor_control: true,
            supports_weapon_control: true,
            supports_jammer_control: false, // no EW hardware
            supports_comm_control: true,
            ..PlatformCapabilities::default()
        }
    }

    fn self_state(id: &str) -> PlatformState {
        use openfang_types::platform::SensorType;
        use openfang_types::platform::{
            Affiliation, Domain, FuelStatus, JammerState, Pose, SensorState, Velocity, WeaponState,
        };
        PlatformState {
            id: id.into(),
            name: id.into(),
            platform_type: "cca".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Air,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 3000.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 200.0,
                vertical_rate_ms: 0.0,
                course_deg: 0.0,
            },
            fuel: FuelStatus {
                remaining_kg: 500.0,
                max_kg: 1000.0,
                consumption_rate_kg_s: 0.05,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![SensorState {
                sensor_id: "radar-1".into(),
                sensor_type: SensorType::Radar,
                mode: "active".into(),
                frequency_hz: None,
                bandwidth_hz: None,
                azimuth_fov_deg: None,
                elevation_fov_deg: None,
                range_max_m: None,
                damage: 0.0,
                host_platform_id: id.into(),
            }],
            onboard_weapons: vec![WeaponState {
                weapon_id: "aam-1".into(),
                weapon_type: "aam".into(),
                quantity_remaining: 4.0,
                max_range_m: None,
                min_range_m: None,
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            }],
            onboard_jammers: vec![JammerState {
                jammer_id: "jam-1".into(),
                host_id: id.into(),
                is_active: true,
                beams: vec![],
            }],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        }
    }

    #[test]
    fn lane_for_class_routes_each_domain() {
        assert_eq!(
            LaneKind::for_class(CommandClass::Motion),
            Some(LaneKind::Nav)
        );
        assert_eq!(
            LaneKind::for_class(CommandClass::Sensor),
            Some(LaneKind::Sensor)
        );
        assert_eq!(
            LaneKind::for_class(CommandClass::ElectronicWarfare),
            Some(LaneKind::Ew)
        );
        assert_eq!(
            LaneKind::for_class(CommandClass::Weapon),
            Some(LaneKind::WeaponSafe)
        );
        assert_eq!(
            LaneKind::for_class(CommandClass::Comm),
            Some(LaneKind::Comm)
        );
        assert_eq!(LaneKind::for_class(CommandClass::Aux), None);
    }

    #[test]
    fn posture_enforcement_routes_through_lanes() {
        use crate::cca_role::posture_for;
        use openfang_types::platform::CcaRole;

        let mut cer = Cerebellum::new(20.0, 64, restrictions());
        cer.set_capabilities(all_caps());
        // Recon safes weapons, jammer off, comms off. Sensor posture is owned
        // by SMS so explicit operator sensor commands are not overwritten here.
        cer.set_posture(posture_for(CcaRole::Recon));
        let intents = cer.posture_intents(&self_state("cca-1"), 0.0);
        assert!(!intents.iter().any(|i| matches!(
            i.command,
            PlatformCommand::SensorOn { .. }
                | PlatformCommand::SensorOff { .. }
                | PlatformCommand::SensorSetMode { .. }
        )));
        assert!(intents
            .iter()
            .any(|i| matches!(i.command, PlatformCommand::JamStop { .. })));
        assert!(intents
            .iter()
            .any(|i| matches!(i.command, PlatformCommand::WeaponSafeAll { .. })));
    }

    #[test]
    fn ew_lane_inert_without_jammer_capability() {
        use crate::cca_role::posture_for;
        use openfang_types::platform::CcaRole;

        let mut cer = Cerebellum::new(20.0, 64, restrictions());
        cer.set_capabilities(no_jammer_caps());
        cer.set_posture(posture_for(CcaRole::Recon));
        let intents = cer.posture_intents(&self_state("cca-1"), 0.0);
        // No EW command survives capability gating.
        assert!(!intents
            .iter()
            .any(|i| matches!(i.command, PlatformCommand::JamStop { .. })));
        let ew = cer
            .lane_statuses()
            .into_iter()
            .find(|s| s.kind == LaneKind::Ew)
            .unwrap();
        assert!(!ew.enabled);
        assert!(ew.posture_gated >= 1, "EW posture command should be gated");
    }

    #[test]
    fn weapon_safe_lane_always_enabled() {
        // Safety lanes must not depend on a capability flag.
        let caps = PlatformCapabilities {
            supports_weapon_control: false,
            ..PlatformCapabilities::default()
        };
        assert!(LaneKind::WeaponSafe.enabled_by(&caps));
        assert!(LaneKind::Survival.enabled_by(&caps));
    }
}
