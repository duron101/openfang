use openfang_types::config::{SensorDisposition, SensorEmconLevel, SensorPolicyConfig};
use openfang_types::platform::SensorType;
use openfang_types::umaa::WeaponReleaseLevel;

use crate::cca_role::EmconLevel;

#[derive(Debug, Clone)]
pub struct SensorPolicyEngine {
    config: SensorPolicyConfig,
}

impl SensorPolicyEngine {
    pub fn new(config: SensorPolicyConfig) -> Self {
        Self { config }
    }

    pub fn from_toml_str(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw).map(Self::new)
    }

    pub fn disposition_for(
        &self,
        sensor_type: SensorType,
        emcon: EmconLevel,
        _roe: WeaponReleaseLevel,
    ) -> SensorDisposition {
        let sensor_emcon = sensor_emcon_level(emcon);
        if let Some(rule) = self
            .config
            .emcon_rules
            .iter()
            .find(|rule| rule.emcon == sensor_emcon && rule.sensor_type == sensor_type)
        {
            return rule.disposition;
        }

        let is_active_emitter = matches!(sensor_type, SensorType::Radar | SensorType::Lidar);
        if is_active_emitter {
            self.config.active_radar_disposition
        } else {
            self.config.passive_sensor_disposition
        }
    }
}

impl Default for SensorPolicyEngine {
    fn default() -> Self {
        Self::new(SensorPolicyConfig::default())
    }
}

fn sensor_emcon_level(emcon: EmconLevel) -> SensorEmconLevel {
    match emcon {
        EmconLevel::Silent => SensorEmconLevel::Silent,
        EmconLevel::Restricted => SensorEmconLevel::Restricted,
        EmconLevel::Normal | EmconLevel::Active => SensorEmconLevel::Unrestricted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_rule_overrides_default_disposition() {
        let engine = SensorPolicyEngine::from_toml_str(
            r#"
            active_radar_disposition = "auto"

            [[emcon_rules]]
            emcon = "restricted"
            sensor_type = "radar"
            disposition = "pending_approval"
        "#,
        )
        .unwrap();

        assert_eq!(
            engine.disposition_for(
                SensorType::Radar,
                EmconLevel::Restricted,
                WeaponReleaseLevel::WeaponsHold
            ),
            SensorDisposition::PendingApproval
        );
    }
}
