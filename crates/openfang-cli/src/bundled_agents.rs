//! Compile-time embedded agent templates.
//!
//! 10 tactical agent profiles (vessel + UAV) embedded via `include_str!()`.
//! Agent sources live under `tactical-assets/agents/`.

/// Returns all bundled agent templates as `(name, toml_content)` pairs.
pub fn bundled_agents() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "tca",
            include_str!("../../../tactical-assets/agents/tca/agent.toml"),
        ),
        (
            "sma",
            include_str!("../../../tactical-assets/agents/sma/agent.toml"),
        ),
        (
            "na",
            include_str!("../../../tactical-assets/agents/na/agent.toml"),
        ),
        (
            "fca",
            include_str!("../../../tactical-assets/agents/fca/agent.toml"),
        ),
        (
            "ca",
            include_str!("../../../tactical-assets/agents/ca/agent.toml"),
        ),
        (
            "fma",
            include_str!("../../../tactical-assets/agents/fma/agent.toml"),
        ),
        (
            "hma",
            include_str!("../../../tactical-assets/agents/hma/agent.toml"),
        ),
        (
            "ora",
            include_str!("../../../tactical-assets/agents/ora/agent.toml"),
        ),
        (
            "uav-cca",
            include_str!("../../../tactical-assets/agents/uav-cca/agent.toml"),
        ),
        (
            "uav-lsuav",
            include_str!("../../../tactical-assets/agents/uav-lsuav/agent.toml"),
        ),
    ]
}

/// Returns bundled `SYSTEM_PROMPT.md` content for agents that ship a file-backed,
/// editable system prompt (tactical + UAV agents). These are installed alongside
/// `agent.toml` so prompts are user-editable and hot-reloadable on disk.
pub fn bundled_agent_prompts() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "tca",
            include_str!("../../../tactical-assets/agents/tca/SYSTEM_PROMPT.md"),
        ),
        (
            "sma",
            include_str!("../../../tactical-assets/agents/sma/SYSTEM_PROMPT.md"),
        ),
        (
            "na",
            include_str!("../../../tactical-assets/agents/na/SYSTEM_PROMPT.md"),
        ),
        (
            "fca",
            include_str!("../../../tactical-assets/agents/fca/SYSTEM_PROMPT.md"),
        ),
        (
            "ca",
            include_str!("../../../tactical-assets/agents/ca/SYSTEM_PROMPT.md"),
        ),
        (
            "fma",
            include_str!("../../../tactical-assets/agents/fma/SYSTEM_PROMPT.md"),
        ),
        (
            "hma",
            include_str!("../../../tactical-assets/agents/hma/SYSTEM_PROMPT.md"),
        ),
        (
            "ora",
            include_str!("../../../tactical-assets/agents/ora/SYSTEM_PROMPT.md"),
        ),
        (
            "uav-cca",
            include_str!("../../../tactical-assets/agents/uav-cca/SYSTEM_PROMPT.md"),
        ),
        (
            "uav-lsuav",
            include_str!("../../../tactical-assets/agents/uav-lsuav/SYSTEM_PROMPT.md"),
        ),
    ]
}

/// Install bundled agent templates to `~/.openfang/agents/`.
/// Skips any template that already exists on disk (user customization preserved).
pub fn install_bundled_agents(agents_dir: &std::path::Path) {
    for (name, content) in bundled_agents() {
        let dest_dir = agents_dir.join(name);
        let dest_file = dest_dir.join("agent.toml");
        if dest_file.exists() {
            continue; // Preserve user customization
        }
        if std::fs::create_dir_all(&dest_dir).is_ok() {
            let _ = std::fs::write(&dest_file, content);
        }
    }

    // Install file-backed system prompts (preserve user edits if already present).
    for (name, prompt) in bundled_agent_prompts() {
        let dest_dir = agents_dir.join(name);
        let dest_file = dest_dir.join("SYSTEM_PROMPT.md");
        if dest_file.exists() {
            continue;
        }
        if std::fs::create_dir_all(&dest_dir).is_ok() {
            let _ = std::fs::write(&dest_file, prompt);
        }
    }
}
