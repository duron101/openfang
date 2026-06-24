//! EngagementGuard — fire-once de-duplication + decision-time weapon checks.
//!
//! The slow cognitive loop writes a `FireAtTarget` into the standing plan; the
//! fast loop replays that plan every tick (~20 Hz). Without a guard the same
//! weapon command is re-dispatched every ~50 ms, flooding the simulator with
//! duplicate engagements (and, in AFSIM, repeatedly firing the same gun).
//!
//! This guard sits at the dispatch chokepoint and enforces, per
//! `(platform, weapon, target)` engagement:
//!   1. **Cooldown de-dup** — after a fire is dispatched, identical fires are
//!      suppressed for `cooldown_secs`. Re-engagement is allowed once the window
//!      elapses (the threat is still present), satisfying "fire once per window".
//!   2. **Decision-time weapon checks** — at the moment of fire, re-validate the
//!      firing platform's ammo (`quantity_remaining`/`is_ready`) and the target's
//!      range against the weapon envelope (`min_range_m`/`max_range_m`), using the
//!      live snapshot. Fires with no ammo or out of range are suppressed.
//!
//! Non-weapon commands (motion/sensor/standing posture) always pass through —
//! they are *meant* to be replayed every tick.

use std::collections::HashMap;
use std::sync::Arc;

use openfang_types::platform::{PlatformCommand, WorldSnapshot};
use openfang_types::wms::ReattackMode;

use crate::wms_policy::WmsPolicyEngine;

/// Default re-engagement cooldown for a `(platform, weapon, track)` tuple.
pub const DEFAULT_ENGAGEMENT_COOLDOWN_SECS: f64 = 20.0;

/// Why a weapon fire was suppressed this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireSuppression {
    /// Identical fire dispatched within the cooldown window.
    Cooldown,
    /// A weapon fired by this platform is still in flight toward this target.
    InFlightWeapon,
    /// Fire-once lock: a strike was already released against this engagement.
    /// Auto re-attack is suppressed until the operator re-designates the target
    /// (`release_engagement`). The slow loop replays the standing plan every
    /// tick, so without this lock the same target is struck on every cooldown
    /// window — the repeated-strike pathology this guard exists to stop.
    AlreadyEngaged,
    /// BDA/event evidence says the target is already neutralized.
    TargetNeutralized,
    /// Firing platform has no ready rounds of this weapon.
    NoAmmo,
    /// Target range is outside the weapon's engagement envelope.
    OutOfRange,
}

impl FireSuppression {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cooldown => "cooldown",
            Self::InFlightWeapon => "in_flight_weapon",
            Self::AlreadyEngaged => "already_engaged",
            Self::TargetNeutralized => "target_neutralized",
            Self::NoAmmo => "no_ammo",
            Self::OutOfRange => "out_of_range",
        }
    }
}

/// One released strike, remembered so the fire-once lock can be released exactly
/// for the originating `(platform, track)` on operator re-designation.
#[derive(Debug, Clone)]
struct FireRecord {
    at: f64,
    platform_id: String,
    track_id: String,
}

/// Per-engagement fire memory + decision-time weapon validation.
#[derive(Debug, Clone)]
pub struct EngagementGuard {
    cooldown_secs: f64,
    weapon_cooldowns_secs: HashMap<String, f64>,
    last_fire: HashMap<(String, String, String), FireRecord>,
    wms_policy: Arc<WmsPolicyEngine>,
}

impl Default for EngagementGuard {
    fn default() -> Self {
        Self::new(DEFAULT_ENGAGEMENT_COOLDOWN_SECS)
    }
}

impl EngagementGuard {
    pub fn new(cooldown_secs: f64) -> Self {
        Self {
            cooldown_secs: cooldown_secs.max(0.0),
            weapon_cooldowns_secs: HashMap::new(),
            last_fire: HashMap::new(),
            wms_policy: Arc::new(WmsPolicyEngine::default()),
        }
    }

    pub fn with_wms_policy(wms_policy: WmsPolicyEngine) -> Self {
        Self {
            cooldown_secs: DEFAULT_ENGAGEMENT_COOLDOWN_SECS,
            weapon_cooldowns_secs: HashMap::new(),
            last_fire: HashMap::new(),
            wms_policy: Arc::new(wms_policy),
        }
    }

    pub fn set_cooldown_secs(&mut self, cooldown_secs: f64) {
        self.cooldown_secs = cooldown_secs.max(0.0);
    }

    pub fn cooldown_secs(&self) -> f64 {
        self.cooldown_secs
    }

    pub fn set_weapon_cooldowns_secs(&mut self, cooldowns: HashMap<String, f64>) {
        self.weapon_cooldowns_secs = cooldowns
            .into_iter()
            .map(|(weapon, secs)| (weapon.to_ascii_lowercase(), secs.max(0.0)))
            .collect();
    }

    /// Decide whether a command may be dispatched this tick.
    ///
    /// Non-weapon commands always return `Ok`. Weapon fires return `Err(reason)`
    /// when suppressed. Range/ammo checks are **fail-open**: when the snapshot
    /// lacks the firing platform/weapon/track (e.g. sparse telemetry or the
    /// `"self"` alias), only the cooldown applies — the guard never blocks a fire
    /// on missing data, it only suppresses on positive evidence.
    pub fn check(
        &self,
        cmd: &PlatformCommand,
        snapshot: Option<&WorldSnapshot>,
        now: f64,
    ) -> Result<(), FireSuppression> {
        let Some((platform_id, weapon_id, track_id)) = fire_fields(cmd) else {
            return Ok(());
        };

        if self.wms_policy.reattack_mode_for(&weapon_id) == ReattackMode::CooldownOnly {
            return self.check_cooldown_only_fire(cmd, snapshot, now);
        }

        let cooldown_key = fire_key(cmd, snapshot);

        if let Some(record) = self.last_fire.get(&cooldown_key) {
            // Already destroyed → never re-fire (also the natural end state).
            if target_is_neutralized(&cooldown_key.2, snapshot) {
                return Err(FireSuppression::TargetNeutralized);
            }
            // Within the cooldown window the replayed fire is a transient
            // duplicate from the ~20 Hz fast loop.
            let cooldown_secs = self.cooldown_secs_for(&weapon_id, snapshot);
            if cooldown_secs > 0.0 && (now - record.at) < cooldown_secs {
                return Err(FireSuppression::Cooldown);
            }
            // A weapon already in flight toward this target — let it resolve.
            if has_in_flight_weapon_to_target(
                &platform_id,
                &weapon_id,
                &track_id,
                &cooldown_key.2,
                snapshot,
            ) {
                return Err(FireSuppression::InFlightWeapon);
            }
            // Fire-once: a strike was already released against this engagement.
            // The standing plan re-issues the same fire every cognitive cycle;
            // do NOT auto re-attack a target that has already been engaged. A
            // fresh strike requires an explicit operator re-designation, which
            // clears this lock via `release_engagement`.
            return Err(FireSuppression::AlreadyEngaged);
        }

        if let Some(snap) = snapshot {
            // The firing platform is the one HOLDING the track (track ids are
            // owner-scoped, e.g. "self:8"); this sidesteps the "self" alias not
            // matching the real telemetry platform id.
            if let Some(platform) = snap
                .platforms
                .iter()
                .find(|p| p.tracks.iter().any(|t| t.track_id == track_id))
            {
                if let Some(weapon) = platform
                    .onboard_weapons
                    .iter()
                    .find(|w| w.weapon_id == weapon_id)
                {
                    if !weapon.is_ready || weapon.quantity_remaining <= 0.0 {
                        return Err(FireSuppression::NoAmmo);
                    }
                    if let Some(track) = platform.tracks.iter().find(|t| t.track_id == track_id) {
                        if let Some(range_m) = track.range_m {
                            let min = weapon.min_range_m.unwrap_or(0.0);
                            let max = weapon.max_range_m.unwrap_or(f64::INFINITY);
                            if range_m < min || range_m > max {
                                return Err(FireSuppression::OutOfRange);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Cooldown-only path for ISR deploys and defensive countermeasures. Never
    /// applies kinetic fire-once, truth-target re-strike locks, or in-flight
    /// munition blocks from other weapon types.
    fn check_cooldown_only_fire(
        &self,
        cmd: &PlatformCommand,
        snapshot: Option<&WorldSnapshot>,
        now: f64,
    ) -> Result<(), FireSuppression> {
        let Some((_platform_id, weapon_id, track_id)) = fire_fields(cmd) else {
            return Ok(());
        };
        let cooldown_key = fire_key(cmd, snapshot);

        if let Some(record) = self.last_fire.get(&cooldown_key) {
            let cooldown_secs = self.cooldown_secs_for(&weapon_id, snapshot);
            if cooldown_secs > 0.0 && now - record.at < cooldown_secs {
                return Err(FireSuppression::Cooldown);
            }
        }

        if let Some(snap) = snapshot {
            if let Some(platform) = snap
                .platforms
                .iter()
                .find(|p| p.tracks.iter().any(|t| t.track_id == track_id))
            {
                if let Some(weapon) = platform
                    .onboard_weapons
                    .iter()
                    .find(|w| w.weapon_id == weapon_id)
                {
                    if !weapon.is_ready || weapon.quantity_remaining <= 0.0 {
                        return Err(FireSuppression::NoAmmo);
                    }
                    if let Some(track) = platform.tracks.iter().find(|t| t.track_id == track_id) {
                        if let Some(range_m) = track.range_m {
                            let min = weapon.min_range_m.unwrap_or(0.0);
                            let max = weapon.max_range_m.unwrap_or(f64::INFINITY);
                            if range_m < min || range_m > max {
                                return Err(FireSuppression::OutOfRange);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Record a fire that was actually dispatched, locking the engagement
    /// (fire-once) and starting its cooldown window.
    pub fn record_fire(
        &mut self,
        cmd: &PlatformCommand,
        snapshot: Option<&WorldSnapshot>,
        now: f64,
    ) {
        if let Some((platform_id, _weapon_id, track_id)) = fire_fields(cmd) {
            let key = fire_key(cmd, snapshot);
            self.last_fire.insert(
                key,
                FireRecord {
                    at: now,
                    platform_id,
                    track_id,
                },
            );
        }
    }

    /// Release the fire-once lock for a `(platform, track)` engagement so a
    /// fresh strike can be authorized. Called when the operator re-designates a
    /// target. Matches every weapon that fired at this exact platform/track.
    pub fn release_engagement(&mut self, platform_id: &str, track_id: &str) {
        self.last_fire
            .retain(|_, rec| !(rec.platform_id == platform_id && rec.track_id == track_id));
    }

    /// Whether a `(platform, track)` engagement currently holds a fire-once lock.
    pub fn is_engaged(&self, platform_id: &str, track_id: &str) -> bool {
        self.last_fire
            .values()
            .any(|rec| rec.platform_id == platform_id && rec.track_id == track_id)
    }

    fn cooldown_secs_for(&self, weapon_id: &str, snapshot: Option<&WorldSnapshot>) -> f64 {
        let weapon_key = weapon_id.to_ascii_lowercase();
        if let Some(secs) = self.weapon_cooldowns_secs.get(&weapon_key) {
            return *secs;
        }

        if let Some(weapon_type) = weapon_type_for(weapon_id, snapshot) {
            let type_key = weapon_type.to_ascii_lowercase();
            if let Some(secs) = self.weapon_cooldowns_secs.get(&type_key) {
                return *secs;
            }
            if let Some(category) = weapon_category(&type_key) {
                if let Some(secs) = self.weapon_cooldowns_secs.get(category) {
                    return *secs;
                }
                return default_category_cooldown_secs(category, self.cooldown_secs);
            }
        }

        if let Some(secs) = self.wms_policy.cooldown_secs_for(weapon_id) {
            return secs;
        }

        if let Some(category) = weapon_category(&weapon_key) {
            if let Some(secs) = self.weapon_cooldowns_secs.get(category) {
                return *secs;
            }
            return default_category_cooldown_secs(category, self.cooldown_secs);
        }

        self.cooldown_secs
    }
}

/// `(platform_id, weapon_id, track_id)` for weapon-release commands; `None` for
/// any non-fire command.
fn fire_fields(cmd: &PlatformCommand) -> Option<(String, String, String)> {
    match cmd {
        PlatformCommand::FireAtTarget {
            platform_id,
            weapon_id,
            track_id,
        }
        | PlatformCommand::FireSalvo {
            platform_id,
            weapon_id,
            track_id,
            ..
        } => Some((platform_id.clone(), weapon_id.clone(), track_id.clone())),
        _ => None,
    }
}

fn fire_key(cmd: &PlatformCommand, snapshot: Option<&WorldSnapshot>) -> (String, String, String) {
    let (platform_id, weapon_id, track_id) =
        fire_fields(cmd).expect("fire_key only called for fire commands");
    let target_key = stable_target_key(&weapon_id, &track_id, snapshot);
    (platform_id, weapon_id, target_key)
}

fn stable_target_key(weapon_id: &str, track_id: &str, snapshot: Option<&WorldSnapshot>) -> String {
    if let Some(snap) = snapshot {
        if let Some(track) = snap
            .platforms
            .iter()
            .flat_map(|platform| platform.tracks.iter())
            .find(|track| track.track_id == track_id)
        {
            if !track.target_name.is_empty() {
                return format!("truth:{}", track.target_name);
            }
        }
    }
    canonical_track_family(weapon_id, track_id)
}

fn canonical_track_family(weapon_id: &str, track_id: &str) -> String {
    let Some((owner, _local)) = track_id.split_once(':') else {
        return format!("track:{track_id}");
    };
    if owner.contains(weapon_id) {
        if let Some((family, suffix)) = owner.rsplit_once('_') {
            if suffix.chars().all(|ch| ch.is_ascii_digit()) {
                return format!("track_family:{family}");
            }
        }
    }
    format!("track:{track_id}")
}

fn target_is_neutralized(target_key: &str, snapshot: Option<&WorldSnapshot>) -> bool {
    let Some(snap) = snapshot else {
        return false;
    };
    let target = target_key
        .strip_prefix("truth:")
        .or_else(|| target_key.strip_prefix("track:"))
        .or_else(|| target_key.strip_prefix("track_family:"))
        .unwrap_or(target_key);
    snap.events.iter().any(|event| match event {
        openfang_types::platform::WorldEvent::PlatformDestroyed { platform_id, .. } => {
            platform_id == target
        }
        openfang_types::platform::WorldEvent::TrackLost { track_id, .. } => track_id == target,
        _ => false,
    })
}

fn munition_name_matches_weapon(weapon_id: &str, name: &str) -> bool {
    let weapon_key = weapon_id.to_ascii_lowercase();
    let name_key = name.to_ascii_lowercase();
    name_key.contains(&weapon_key) || weapon_category(&name_key) == weapon_category(&weapon_key)
}

fn has_in_flight_weapon_to_target(
    platform_id: &str,
    weapon_id: &str,
    track_id: &str,
    target_key: &str,
    snapshot: Option<&WorldSnapshot>,
) -> bool {
    let Some(snap) = snapshot else {
        return false;
    };
    let target_ids = target_aliases(target_key, track_id, snap);

    // Check traditional active munitions
    let has_active_munition = snap.active_munitions.iter().any(|munition| {
        let Some(target_id) = munition.target_id.as_deref() else {
            return false;
        };
        host_matches(platform_id, munition.host_platform_id.as_deref())
            && munition_matches_weapon(weapon_id, munition)
            && target_ids.iter().any(|alias| alias == target_id)
    });
    if has_active_munition {
        return true;
    }

    // Check platform-based munitions (AFSIM represents loitering munitions as independent platforms)
    snap.platforms.iter().any(|p| {
        // Is it a munition/weapon platform?
        let is_munition = p.platform_type == "weapon"
            || p.platform_type == "munition"
            || p.platform_type == "loiter"
            || p.id.to_lowercase().contains("loiter")
            || p.name.to_lowercase().contains("loiter")
            || p.id.to_lowercase().contains("weapon")
            || p.name.to_lowercase().contains("weapon");
        if !is_munition {
            return false;
        }

        // Does the parent match the firing platform?
        let host_ok = host_matches(platform_id, p.commander.as_deref())
            || p.id.to_lowercase().contains(platform_id);
        if !host_ok {
            return false;
        }

        // Does the weapon match?
        let weapon_ok = munition_name_matches_weapon(weapon_id, &p.id)
            || munition_name_matches_weapon(weapon_id, &p.name)
            || munition_name_matches_weapon(weapon_id, &p.platform_type);
        if !weapon_ok {
            return false;
        }

        // Does the target match?
        if let Some(target_id) = p.current_target.as_deref() {
            target_ids.iter().any(|alias| alias == target_id)
        } else {
            false
        }
    })
}

fn target_aliases(target_key: &str, track_id: &str, snap: &WorldSnapshot) -> Vec<String> {
    let target = target_key
        .strip_prefix("truth:")
        .or_else(|| target_key.strip_prefix("track:"))
        .or_else(|| target_key.strip_prefix("track_family:"))
        .unwrap_or(target_key);
    let mut aliases = vec![track_id.to_string(), target.to_string()];
    for track in snap
        .platforms
        .iter()
        .flat_map(|platform| platform.tracks.iter())
    {
        if (track.track_id == track_id || track.track_id == target) && !track.target_name.is_empty()
        {
            aliases.push(track.target_name.clone());
        }
        if !track.target_name.is_empty() && track.target_name == target {
            aliases.push(track.track_id.clone());
        }
    }
    aliases.sort();
    aliases.dedup();
    aliases
}

fn host_matches(platform_id: &str, host_platform_id: Option<&str>) -> bool {
    let Some(host) = host_platform_id else {
        return true;
    };
    platform_id == "self" || host == platform_id || host == "self"
}

fn munition_matches_weapon(
    weapon_id: &str,
    munition: &openfang_types::platform::ActiveMunition,
) -> bool {
    let weapon_key = weapon_id.to_ascii_lowercase();
    let munition_id = munition.munition_id.to_ascii_lowercase();
    let munition_type = munition.munition_type.to_ascii_lowercase();
    munition_id.contains(&weapon_key)
        || munition_type == weapon_key
        || weapon_category(&munition_type) == weapon_category(&weapon_key)
}

fn weapon_type_for(weapon_id: &str, snapshot: Option<&WorldSnapshot>) -> Option<String> {
    snapshot?
        .platforms
        .iter()
        .flat_map(|platform| platform.onboard_weapons.iter())
        .find(|weapon| weapon.weapon_id == weapon_id)
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

fn default_category_cooldown_secs(category: &str, fallback_secs: f64) -> f64 {
    match category {
        "gun" => fallback_secs.min(2.0),
        "loiter" | "missile" | "torpedo" => fallback_secs.max(60.0),
        _ => fallback_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::{
        ActiveMunition, Affiliation, PlatformState, Track, WeaponState, WorldSnapshot,
    };

    fn fire(track: &str) -> PlatformCommand {
        PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "gun_30mm".into(),
            track_id: track.into(),
        }
    }

    fn track(track_id: &str, range_m: Option<f64>) -> Track {
        Track {
            track_id: track_id.into(),
            target_name: "blue_patrol_3".into(),
            classification: "boat".into(),
            affiliation: Affiliation::Red,
            iff: "foe".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m,
            bearing_deg: None,
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }
    }

    fn snapshot_with(weapon: WeaponState, track_range_m: Option<f64>) -> WorldSnapshot {
        let mut own = PlatformState::minimal("usv_01");
        own.onboard_weapons = vec![weapon];
        own.tracks = vec![track("self:8", track_range_m)];
        WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![own],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn weapon(qty: f64, ready: bool, min: Option<f64>, max: Option<f64>) -> WeaponState {
        WeaponState {
            weapon_id: "gun_30mm".into(),
            weapon_type: "gun".into(),
            quantity_remaining: qty,
            max_range_m: max,
            min_range_m: min,
            guidance_type: None,
            speed_ms: None,
            is_ready: ready,
            quantity_from_snapshot: true,
        }
    }

    #[test]
    fn non_weapon_commands_always_pass() {
        let guard = EngagementGuard::new(20.0);
        let cmd = PlatformCommand::SetHeading {
            platform_id: "self".into(),
            heading_deg: 90.0,
            speed_ms: None,
            turn_direction: None,
        };
        assert!(guard.check(&cmd, None, 0.0).is_ok());
    }

    #[test]
    fn cooldown_then_fire_once_lock_until_redesignation() {
        let mut guard = EngagementGuard::new(20.0);
        let cmd = fire("self:8");
        assert!(guard.check(&cmd, None, 100.0).is_ok());
        guard.record_fire(&cmd, None, 100.0);
        // Within the (gun) cooldown window: transient duplicate.
        assert_eq!(
            guard.check(&cmd, None, 100.05),
            Err(FireSuppression::Cooldown)
        );
        assert_eq!(
            guard.check(&cmd, None, 101.9),
            Err(FireSuppression::Cooldown)
        );
        // After the cooldown, fire-once keeps the engagement locked — the slow
        // loop must NOT auto re-strike the same target.
        assert_eq!(
            guard.check(&cmd, None, 102.1),
            Err(FireSuppression::AlreadyEngaged)
        );
        assert_eq!(
            guard.check(&cmd, None, 9999.0),
            Err(FireSuppression::AlreadyEngaged)
        );
        // Operator re-designates the target → fresh strike allowed again.
        guard.release_engagement("self", "self:8");
        assert!(guard.check(&cmd, None, 9999.0).is_ok());
    }

    #[test]
    fn different_track_is_independent() {
        let mut guard = EngagementGuard::new(20.0);
        let a = fire("self:8");
        guard.record_fire(&a, None, 100.0);
        assert_eq!(guard.check(&a, None, 100.1), Err(FireSuppression::Cooldown));
        // A different target is a separate engagement.
        assert!(guard.check(&fire("self:9"), None, 100.1).is_ok());
        assert!(guard.is_engaged("self", "self:8"));
        assert!(!guard.is_engaged("self", "self:9"));
    }

    #[test]
    fn suppresses_when_out_of_ammo() {
        let guard = EngagementGuard::new(20.0);
        let snap = snapshot_with(weapon(0.0, true, None, None), Some(1000.0));
        assert_eq!(
            guard.check(&fire("self:8"), Some(&snap), 0.0),
            Err(FireSuppression::NoAmmo)
        );
    }

    #[test]
    fn suppresses_when_out_of_range() {
        let guard = EngagementGuard::new(20.0);
        let snap = snapshot_with(weapon(10.0, true, Some(0.0), Some(500.0)), Some(1000.0));
        assert_eq!(
            guard.check(&fire("self:8"), Some(&snap), 0.0),
            Err(FireSuppression::OutOfRange)
        );
    }

    #[test]
    fn allows_in_range_with_ammo() {
        let guard = EngagementGuard::new(20.0);
        let snap = snapshot_with(weapon(10.0, true, Some(0.0), Some(5000.0)), Some(1000.0));
        assert!(guard.check(&fire("self:8"), Some(&snap), 0.0).is_ok());
    }

    #[test]
    fn fail_open_when_platform_not_in_snapshot() {
        // Missing telemetry must not block a fire — only the cooldown applies.
        let guard = EngagementGuard::new(20.0);
        let empty = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        assert!(guard.check(&fire("self:8"), Some(&empty), 0.0).is_ok());
    }

    #[test]
    fn same_truth_target_stays_locked_until_redesignation() {
        // Two distinct track ids that resolve to the SAME truth target are one
        // engagement: firing once locks it; a second auto-strike is suppressed.
        let mut guard = EngagementGuard::new(20.0);
        let first_snap = snapshot_with(weapon(10.0, true, Some(0.0), Some(5000.0)), Some(1000.0));
        let first = fire("self:8");
        guard.record_fire(&first, Some(&first_snap), 100.0);

        let mut second_snap =
            snapshot_with(weapon(10.0, true, Some(0.0), Some(5000.0)), Some(1100.0));
        second_snap.platforms[0].tracks = vec![track("self:9", Some(1100.0))];
        // Same truth target_name ("blue_patrol_3") → fire-once lock holds.
        assert_eq!(
            guard.check(&fire("self:9"), Some(&second_snap), 130.0),
            Err(FireSuppression::AlreadyEngaged)
        );
        // Re-designating the original track frees the shared truth engagement.
        guard.release_engagement("self", "self:8");
        assert!(guard
            .check(&fire("self:9"), Some(&second_snap), 130.0)
            .is_ok());
    }

    #[test]
    fn loiter_track_family_locks_until_redesignation_when_truth_name_missing() {
        let mut guard = EngagementGuard::new(20.0);
        let first = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave3".into(),
            track_id: "self_loiter_wave3_8:1".into(),
        };
        guard.record_fire(&first, None, 100.0);

        let second = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave3".into(),
            track_id: "self_loiter_wave3_9:1".into(),
        };
        assert_eq!(
            guard.check(&second, None, 170.0),
            Err(FireSuppression::AlreadyEngaged)
        );
    }

    #[test]
    fn in_flight_weapon_to_existing_track_blocks_re_fire_after_cooldown() {
        let mut guard = EngagementGuard::new(2.0);
        let snap = snapshot_with(weapon(10.0, true, Some(0.0), Some(5000.0)), Some(1000.0));
        let cmd = fire("self:8");
        guard.record_fire(&cmd, Some(&snap), 100.0);

        let mut bda_snap = snap.clone();
        bda_snap.active_munitions = vec![ActiveMunition {
            munition_id: "self_gun_30mm_1".into(),
            munition_type: "gun".into(),
            affiliation: Affiliation::Red,
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            target_id: Some("blue_patrol_3".into()),
            time_to_impact_s: None,
            host_platform_id: Some("self".into()),
        }];

        assert_eq!(
            guard.check(&cmd, Some(&bda_snap), 103.0),
            Err(FireSuppression::InFlightWeapon)
        );
        // Once the in-flight weapon clears, the fire-once lock still holds: no
        // automatic re-strike without an operator re-designation.
        bda_snap.active_munitions.clear();
        assert_eq!(
            guard.check(&cmd, Some(&bda_snap), 103.0),
            Err(FireSuppression::AlreadyEngaged)
        );
        guard.release_engagement("self", "self:8");
        assert!(guard.check(&cmd, Some(&bda_snap), 103.0).is_ok());
    }

    #[test]
    fn configured_weapon_cooldown_overrides_category_default() {
        let mut guard = EngagementGuard::new(20.0);
        guard.set_weapon_cooldowns_secs(HashMap::from([("gun_30mm".into(), 5.0)]));
        let cmd = fire("self:8");
        guard.record_fire(&cmd, None, 100.0);

        assert_eq!(
            guard.check(&cmd, None, 104.0),
            Err(FireSuppression::Cooldown)
        );
        assert_eq!(
            guard.check(&cmd, None, 106.0),
            Err(FireSuppression::AlreadyEngaged)
        );
    }

    #[test]
    fn isr_deploy_not_blocked_by_kinetic_engagement_lock_or_in_flight() {
        let mut guard = EngagementGuard::new(60.0);
        let mut own = PlatformState::minimal("usv_01");
        own.onboard_weapons = vec![
            WeaponState {
                weapon_id: "loiter_wave3".into(),
                weapon_type: "loiter".into(),
                quantity_remaining: 5.0,
                max_range_m: Some(20_000.0),
                min_range_m: Some(0.0),
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            },
            WeaponState {
                weapon_id: "scout_uav_slot".into(),
                weapon_type: "SCOUT_UAV_SLOT".into(),
                quantity_remaining: 2.0,
                max_range_m: Some(30_000.0),
                min_range_m: Some(0.0),
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            },
        ];
        own.tracks = vec![track("self:8", Some(1000.0))];
        let snap = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![own],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        let loiter = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave3".into(),
            track_id: "self:8".into(),
        };
        guard.record_fire(&loiter, Some(&snap), 100.0);

        let mut in_flight = snap.clone();
        in_flight.active_munitions = vec![ActiveMunition {
            munition_id: "self_loiter_wave3_1".into(),
            munition_type: "loiter".into(),
            affiliation: Affiliation::Red,
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            target_id: Some("blue_patrol_3".into()),
            time_to_impact_s: None,
            host_platform_id: Some("self".into()),
        }];

        let scout = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "scout_uav_slot".into(),
            track_id: "self:8".into(),
        };
        assert!(
            guard.check(&scout, Some(&in_flight), 200.0).is_ok(),
            "ISR deploy must not inherit kinetic in-flight / fire-once locks"
        );
        assert_eq!(
            guard.check(&loiter, Some(&in_flight), 200.0),
            Err(FireSuppression::InFlightWeapon),
            "kinetic re-fire still blocked while munition is in flight"
        );
    }

    #[test]
    fn isr_deploy_repeats_after_antispam_not_fire_once() {
        let mut guard = EngagementGuard::new(60.0);
        let scout = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "scout_uav_slot".into(),
            track_id: "self:8".into(),
        };
        guard.record_fire(&scout, None, 100.0);
        assert_eq!(
            guard.check(&scout, None, 101.0),
            Err(FireSuppression::Cooldown)
        );
        assert!(
            guard.check(&scout, None, 103.0).is_ok(),
            "ISR deploy may repeat after short anti-spam; not fire-once locked"
        );
    }

    #[test]
    fn j7_uav_weapon_repeats_after_isr_cooldown_not_fire_once() {
        let mut guard = EngagementGuard::new(60.0);
        let j7 = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "J7_UAV_WEAPON".into(),
            track_id: "self:8".into(),
        };

        guard.record_fire(&j7, None, 100.0);

        assert_eq!(
            guard.check(&j7, None, 101.0),
            Err(FireSuppression::Cooldown)
        );
        assert!(
            guard.check(&j7, None, 103.0).is_ok(),
            "J7 UAV deploy may repeat after ISR cooldown; it must not be fire-once locked"
        );
    }

    #[test]
    fn wms_policy_cooldown_only_weapon_repeats_after_configured_window() {
        let mut guard =
            EngagementGuard::with_wms_policy(crate::wms_policy::WmsPolicyEngine::default());
        let cmd = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "chaff_launcher".into(),
            track_id: "self:8".into(),
        };

        guard.record_fire(&cmd, None, 100.0);

        assert_eq!(
            guard.check(&cmd, None, 100.5),
            Err(FireSuppression::Cooldown)
        );
        assert!(
            guard.check(&cmd, None, 101.1).is_ok(),
            "countermeasures use WMS cooldown_only instead of fire-once"
        );
    }

    #[test]
    fn wms_policy_fire_once_weapon_stays_locked_after_cooldown() {
        let mut guard =
            EngagementGuard::with_wms_policy(crate::wms_policy::WmsPolicyEngine::default());
        let cmd = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave3".into(),
            track_id: "self:8".into(),
        };

        guard.record_fire(&cmd, None, 100.0);

        assert_eq!(
            guard.check(&cmd, None, 159.0),
            Err(FireSuppression::Cooldown)
        );
        assert_eq!(
            guard.check(&cmd, None, 161.0),
            Err(FireSuppression::AlreadyEngaged)
        );
    }
}
