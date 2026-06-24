//! Bundled skills — compile-time embedded SKILL.md files.
//!
//! Ships 4 tactical prompt-only skills inside the OpenFang binary via `include_str!()`.
//! Skill sources live under `tactical-assets/skills/`. User-installed skills override bundled ones.

use crate::openclaw_compat::convert_skillmd_str;
use crate::SkillManifest;

/// Return all bundled (name, raw SKILL.md content) pairs.
pub fn bundled_skills() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "tactical-engagement",
            include_str!("../../../tactical-assets/skills/tactical-engagement/SKILL.md"),
        ),
        (
            "electronic-warfare",
            include_str!("../../../tactical-assets/skills/electronic-warfare/SKILL.md"),
        ),
        (
            "fleet-coordination",
            include_str!("../../../tactical-assets/skills/fleet-coordination/SKILL.md"),
        ),
        (
            "maritime-navigation",
            include_str!("../../../tactical-assets/skills/maritime-navigation/SKILL.md"),
        ),
    ]
}

/// Parse a bundled SKILL.md into a `SkillManifest`.
pub fn parse_bundled(name: &str, content: &str) -> Result<SkillManifest, crate::SkillError> {
    let converted = convert_skillmd_str(name, content)?;
    Ok(converted.manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_skills_count() {
        let skills = bundled_skills();
        assert_eq!(skills.len(), 4, "Expected 4 bundled tactical skills");
    }

    #[test]
    fn test_all_bundled_skills_parse() {
        let skills = bundled_skills();
        for (name, content) in &skills {
            let result = parse_bundled(name, content);
            assert!(
                result.is_ok(),
                "Failed to parse bundled skill '{}': {:?}",
                name,
                result.err()
            );
            let manifest = result.unwrap();
            assert!(
                !manifest.skill.name.is_empty(),
                "Bundled skill '{}' has empty name",
                name
            );
            assert!(
                !manifest.skill.description.is_empty(),
                "Bundled skill '{}' has empty description",
                name
            );
            assert!(
                manifest.prompt_context.is_some(),
                "Bundled skill '{}' has no prompt context",
                name
            );
            assert_eq!(
                manifest.source,
                Some(crate::SkillSource::Bundled),
                "Bundled skill '{}' should have Bundled source",
                name
            );
        }
    }

    #[test]
    fn test_bundled_skills_pass_security_scan() {
        use crate::verify::SkillVerifier;

        let skills = bundled_skills();
        for (name, content) in &skills {
            let manifest = parse_bundled(name, content).unwrap();
            if let Some(ref ctx) = manifest.prompt_context {
                let warnings = SkillVerifier::scan_prompt_content(ctx);
                let has_critical = warnings
                    .iter()
                    .any(|w| matches!(w.severity, crate::verify::WarningSeverity::Critical));
                assert!(
                    !has_critical,
                    "Bundled skill '{}' has critical security warnings: {:?}",
                    name, warnings
                );
            }
        }
    }

    #[test]
    fn test_user_skill_overrides_bundled() {
        use crate::registry::SkillRegistry;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let mut registry = SkillRegistry::new(dir.path().to_path_buf());

        // Load bundled
        let bundled_count = registry.load_bundled();
        assert!(bundled_count > 0);

        // Create a user skill with the same name as a bundled one
        let skill_dir = dir.path().join("tactical-engagement");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("skill.toml"),
            r#"
[skill]
name = "tactical-engagement"
version = "99.0.0"
description = "User-customized tactical engagement skill"

[runtime]
type = "promptonly"
entry = ""
"#,
        )
        .unwrap();

        // Load user skills — should override the bundled one
        registry.load_all().unwrap();

        let skill = registry.get("tactical-engagement").unwrap();
        assert_eq!(
            skill.manifest.skill.version, "99.0.0",
            "User skill should override bundled skill"
        );
    }
}
