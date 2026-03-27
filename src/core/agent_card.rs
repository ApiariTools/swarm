use a2a_types::{AgentCapabilities, AgentCard, AgentSkill};

/// Build an A2A AgentCard for a swarm worker.
///
/// `worker_id` — short worker identifier (e.g. "apiari-d743")
/// `repo`      — repository name the worker is operating on
/// `agent`     — agent type string ("claude", "codex", "gemini")
/// `profile`   — raw markdown profile content (used to derive skills)
#[allow(dead_code)]
pub fn build_agent_card(worker_id: &str, repo: &str, agent: &str, profile: &str) -> AgentCard {
    let skills = parse_skills_from_profile(profile);

    AgentCard::new(
        worker_id,
        format!("Swarm worker for {repo} ({agent})"),
        env!("CARGO_PKG_VERSION"),
        format!("http://localhost:0/workers/{worker_id}"),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(false),
        push_notifications: Some(false),
        state_transition_history: Some(false),
        extensions: vec![],
    })
    .with_skills(skills)
}

/// Parse markdown profile content into A2A AgentSkill entries.
///
/// Extracts `## Heading` sections and uses the heading as the skill name and the
/// body text (up to the next heading) as the description.
fn parse_skills_from_profile(profile: &str) -> Vec<AgentSkill> {
    let mut skills = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_body = String::new();

    for line in profile.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            // Flush previous section
            if let Some(name) = current_heading.take() {
                skills.push(make_skill(&name, &current_body));
                current_body.clear();
            }
            current_heading = Some(heading.trim().to_string());
        } else if current_heading.is_some() && (!current_body.is_empty() || !line.trim().is_empty())
        {
            if !current_body.is_empty() {
                current_body.push('\n');
            }
            current_body.push_str(line);
        }
    }

    // Flush last section
    if let Some(name) = current_heading {
        skills.push(make_skill(&name, &current_body));
    }

    skills
}

fn make_skill(name: &str, body: &str) -> AgentSkill {
    let id = name
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "-")
        .trim_matches('-')
        .to_string();

    AgentSkill {
        id,
        name: name.to_string(),
        description: body.trim().to_string(),
        tags: vec!["swarm".to_string()],
        examples: vec![],
        input_modes: vec![],
        output_modes: vec![],
        security: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_agent_card_basic() {
        let card = build_agent_card(
            "apiari-d743",
            "hive",
            "claude",
            "# Profile\n## Rules\nDo X.",
        );
        assert_eq!(card.name, "apiari-d743");
        assert_eq!(card.description, "Swarm worker for hive (claude)");
        assert_eq!(card.capabilities.streaming, Some(false));
        assert_eq!(card.capabilities.push_notifications, Some(false));
        assert!(card.url.contains("apiari-d743"));
    }

    #[test]
    fn build_agent_card_parses_skills() {
        let profile =
            "# Worker Profile\n\n## Rules\nFollow rules.\n\n## Scope Discipline\nStay focused.\n";
        let card = build_agent_card("w-1", "repo", "codex", profile);
        assert_eq!(card.skills.len(), 2);
        assert_eq!(card.skills[0].name, "Rules");
        assert_eq!(card.skills[0].id, "rules");
        assert_eq!(card.skills[1].name, "Scope Discipline");
        assert_eq!(card.skills[1].id, "scope-discipline");
    }

    #[test]
    fn default_profile_produces_skills() {
        let profile = crate::core::profile::DEFAULT_PROFILE;
        let card = build_agent_card("test-1", "repo", "claude", profile);
        assert!(
            !card.skills.is_empty(),
            "default profile should produce at least one skill"
        );
    }

    #[test]
    fn agent_card_serializes_to_json() {
        let card = build_agent_card("w-1", "repo", "claude", "## Coding\nWrite code.");
        let json = serde_json::to_value(&card).expect("serialize");
        assert_eq!(json["name"], "w-1");
        assert!(json["skills"].as_array().unwrap().len() > 0);
    }

    #[test]
    fn skills_have_swarm_tag() {
        let card = build_agent_card("w-1", "repo", "gemini", "## Testing\nRun tests.");
        for skill in &card.skills {
            assert!(skill.tags.contains(&"swarm".to_string()));
        }
    }

    #[test]
    fn empty_profile_produces_no_skills() {
        let card = build_agent_card("w-1", "repo", "claude", "# Just a title\nNo sections.");
        assert!(card.skills.is_empty());
    }
}
