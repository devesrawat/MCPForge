//! Curated safe-defaults deny-tools presets for popular MCP servers.
//!
//! Tool names are verified against real server source (see
//! docs/superpowers/specs/2026-07-26-mcp-forge-safe-defaults-policy-packs-design.md
//! for exactly which commit/tag each list was extracted from), not
//! guessed from memory. The rule: deny every tool that mutates external
//! state, leave every read-only tool allowed by omission.

/// `@modelcontextprotocol/server-github` (verified against the
/// resolvable `2025.4.8` tag of `modelcontextprotocol/servers` — the
/// package is deprecated and its source is no longer on that repo's
/// `main` branch, which is why a tag rather than `main` is cited here).
pub const GITHUB_DENY_TOOLS: &[&str] = &[
    "create_or_update_file",
    "push_files",
    "create_repository",
    "fork_repository",
    "create_branch",
    "create_issue",
    "update_issue",
    "add_issue_comment",
    "create_pull_request",
    "create_pull_request_review",
    "update_pull_request_branch",
    "merge_pull_request",
];

/// `@modelcontextprotocol/server-filesystem` (verified against
/// `main` of `modelcontextprotocol/servers`).
pub const FILESYSTEM_DENY_TOOLS: &[&str] =
    &["write_file", "edit_file", "create_directory", "move_file"];

/// Look up a preset's deny list by name. Returns `None` for unknown
/// names — callers should surface `known_presets()` in the error.
pub fn preset_deny_tools(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "github" => Some(GITHUB_DENY_TOOLS),
        "filesystem" => Some(FILESYSTEM_DENY_TOOLS),
        _ => None,
    }
}

/// Names of every available preset, for error messages and `--help`-style
/// listings.
pub fn known_presets() -> &'static [&'static str] {
    &["github", "filesystem"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_deny_tools_returns_github_list() {
        let tools = preset_deny_tools("github").expect("github preset exists");
        assert_eq!(tools, GITHUB_DENY_TOOLS);
        assert!(tools.contains(&"merge_pull_request"));
        assert!(tools.contains(&"create_or_update_file"));
    }

    #[test]
    fn preset_deny_tools_returns_filesystem_list() {
        let tools = preset_deny_tools("filesystem").expect("filesystem preset exists");
        assert_eq!(tools, FILESYSTEM_DENY_TOOLS);
        assert!(tools.contains(&"write_file"));
    }

    #[test]
    fn preset_deny_tools_returns_none_for_unknown_name() {
        assert_eq!(preset_deny_tools("postgres"), None);
        assert_eq!(preset_deny_tools(""), None);
        assert_eq!(preset_deny_tools("GitHub"), None); // case-sensitive, exact match only
    }

    #[test]
    fn known_presets_lists_both_available_names() {
        let presets = known_presets();
        assert_eq!(presets, &["github", "filesystem"]);
    }

    /// Guards against `known_presets()` advertising a name that
    /// `preset_deny_tools()` doesn't actually recognize (e.g. a future
    /// preset added to one list but not the other) — such drift would
    /// otherwise surface only as a confusing "unknown preset" error for
    /// a name the CLI's own error message just told the user was valid.
    #[test]
    fn every_known_preset_has_a_nonempty_deny_list() {
        for name in known_presets() {
            let tools = preset_deny_tools(name);
            assert!(
                tools.is_some_and(|list| !list.is_empty()),
                "known preset '{}' has no matching non-empty deny list",
                name
            );
        }
    }
}
