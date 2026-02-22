//! Shell quoting and sanitization helpers.

/// Wrap a string in single quotes for safe embedding in a shell command.
///
/// Single quotes inside the string are handled via the `'\''` idiom:
/// end the current single-quoted segment, insert an escaped single quote,
/// then start a new single-quoted segment.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Sanitize a string for use as a branch name or directory name.
///
/// - Lowercases everything
/// - Replaces non-alphanumeric characters with hyphens
/// - Strips leading/trailing hyphens
/// - Truncates to 40 characters
pub fn sanitize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
        .chars()
        .take(40)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn test_shell_quote_with_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_quote_with_single_quotes() {
        assert_eq!(shell_quote("it's fine"), "'it'\\''s fine'");
    }

    #[test]
    fn test_shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_sanitize_basic() {
        assert_eq!(sanitize("Fix the bug"), "fix-the-bug");
    }

    #[test]
    fn test_sanitize_special_chars() {
        assert_eq!(sanitize("add user auth (v2)"), "add-user-auth--v2");
    }

    #[test]
    fn test_sanitize_strips_leading_trailing_hyphens() {
        assert_eq!(sanitize("--hello--"), "hello");
    }

    #[test]
    fn test_sanitize_truncates_to_40() {
        let long = "a".repeat(50);
        assert_eq!(sanitize(&long).len(), 40);
    }

    #[test]
    fn test_sanitize_empty() {
        assert_eq!(sanitize(""), "");
    }
}
