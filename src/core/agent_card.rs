use a2a_types::{AgentCapabilities, AgentCard, AgentSkill};
use std::fmt::Write;

/// Build an A2A AgentCard for a swarm worker.
///
/// `worker_id` — short worker identifier (e.g. "apiari-d743")
/// `repo`      — repository name the worker is operating on
/// `agent`     — agent type string ("claude", "codex", "gemini")
/// `profile`   — raw markdown profile content (used to derive skills)
///
/// The URL defaults to `http://localhost:0/a2a/workers/<id>` (port 0 = not yet
/// allocated). The daemon overrides `card.url` with the real A2A HTTP port
/// after the server binds (see `handle_request` / `CreateWorker`).
pub fn build_agent_card(worker_id: &str, repo: &str, agent: &str, profile: &str) -> AgentCard {
    let skills = parse_skills_from_profile(profile);

    // Default URL uses port 0 as a placeholder. The daemon overrides this
    // with the real A2A HTTP port (see `handle_request` / `CreateWorker`).
    let encoded_id = url_encode_path_segment(worker_id);
    AgentCard::new(
        worker_id,
        format!("Swarm worker for {repo} ({agent})"),
        env!("CARGO_PKG_VERSION"),
        format!("http://localhost:0/a2a/workers/{encoded_id}"),
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
/// body text (up to the next heading) as the description. Empty headings are skipped.
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
            let trimmed = heading.trim();
            if !trimmed.is_empty() {
                current_heading = Some(trimmed.to_string());
            }
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

/// Percent-encode a worker ID for use as a URL path segment (RFC 3986 unreserved characters).
pub fn url_encode_worker_id(s: &str) -> String {
    url_encode_path_segment(s)
}

/// Percent-encode a string for use as a URL path segment (RFC 3986 unreserved characters).
fn url_encode_path_segment(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            // RFC 3986 unreserved characters
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char);
            }
            _ => {
                let _ = write!(encoded, "%{:02X}", b);
            }
        }
    }
    encoded
}

fn make_skill(name: &str, body: &str) -> AgentSkill {
    let raw = name
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "-");
    // Collapse repeated dashes and trim leading/trailing dashes.
    let mut id = String::new();
    for ch in raw.chars() {
        if ch == '-' && id.ends_with('-') {
            continue;
        }
        id.push(ch);
    }
    let id = id.trim_matches('-').to_string();
    // Fall back to a stable id if the heading had no alphanumeric chars.
    let id = if id.is_empty() {
        format!("skill-{:x}", name.len())
    } else {
        id
    };

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

    #[test]
    fn empty_heading_is_skipped() {
        let card = build_agent_card("w-1", "repo", "claude", "##  \n## Real\nContent.");
        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].name, "Real");
    }
}
