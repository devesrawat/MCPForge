//! Destructive-pattern detection for tool names and arguments.
//!
//! Structurally mirrors `injection.rs`: scans for patterns that suggest
//! a tool call is about to delete, drop, force, or otherwise destroy
//! data, and can operate in warn mode (log + forward) or block mode
//! (log + reject). Unlike prompt injection, this has no "scan the
//! result" side — a destructive pattern is meaningful on what an agent
//! is about to *do* (tool name + arguments), not on what a tool
//! *returned*.
//!
//! This is defense-in-depth against unsophisticated or accidental
//! destructive calls, not a security boundary against an adversarial
//! agent or MCP server: both matchers are trivially evadable (`rm -fr`,
//! encoded payloads, non-English keywords). Don't rely on it as a hard
//! security control.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveMode {
    /// Log suspicious patterns but allow the tool call
    Warn,
    /// Log suspicious patterns and block the tool call
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveLocation {
    ToolName,
    Argument,
}

#[derive(Debug, Clone)]
pub struct DestructiveAlert {
    pub matched: String,
    pub location: DestructiveLocation,
}

#[derive(Debug)]
pub struct DestructivePatternDetector {
    mode: DestructiveMode,
}

/// Case-insensitive keywords checked against `_`-separated tokens of a
/// tool name (e.g. `delete_file` tokenizes to `["delete", "file"]`).
/// Matching whole tokens rather than substrings avoids false positives
/// where the keyword merely appears inside an unrelated word —
/// `enforce_policy`, `airdrop_tokens`, `undelete_file` (the semantic
/// opposite of destructive), and `workforce_report` do not tokenize to
/// any of these words even though they contain them as substrings.
///
/// `truncate` is deliberately excluded here (kept only in
/// `get_argument_patterns`'s `truncate\s+table` pattern, where context
/// disambiguates it): a tool named `truncate_string`/`truncate_text` is
/// a common, harmless text utility, and token matching alone can't tell
/// it apart from a genuinely destructive `truncate_table`.
const DESTRUCTIVE_NAME_KEYWORDS: &[&str] = &["delete", "drop", "destroy", "purge", "wipe", "force"];

fn get_argument_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"(?i)\brm\s+-rf\b").unwrap(),
            Regex::new(r"(?i)\bdrop\s+table\b").unwrap(),
            Regex::new(r"(?i)\bdrop\s+database\b").unwrap(),
            Regex::new(r"(?i)\btruncate\s+table\b").unwrap(),
            // Followed by whitespace or end-of-string, not `--force\b`,
            // which would also match `--force-recreate`/`--force-rm`/
            // `--force-color` (the `\b` boundary sits at the hyphen
            // regardless of what follows it).
            Regex::new(r"(?i)--force(?:\s|$)").unwrap(),
        ]
    })
}

impl DestructivePatternDetector {
    pub fn new(mode: DestructiveMode) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> DestructiveMode {
        self.mode
    }

    /// Check a tool name for destructive keywords. Tokenizes on `_` and
    /// matches whole tokens (case-insensitive) rather than substrings —
    /// see `DESTRUCTIVE_NAME_KEYWORDS`'s doc comment for why.
    pub fn scan_tool_name(&self, tool_name: &str) -> Option<DestructiveAlert> {
        let lower = tool_name.to_ascii_lowercase();
        for token in lower.split('_') {
            if let Some(&keyword) = DESTRUCTIVE_NAME_KEYWORDS.iter().find(|&&k| k == token) {
                return Some(DestructiveAlert {
                    matched: keyword.to_owned(),
                    location: DestructiveLocation::ToolName,
                });
            }
        }
        None
    }

    /// Recursively scan tool arguments (JSON object/array/string) for
    /// destructive-command-shaped patterns. Sorted key walk for
    /// determinism, matching `InjectionDetector::scan_arguments`.
    pub fn scan_arguments(&self, args: &Value) -> Option<DestructiveAlert> {
        match args {
            Value::String(s) => self.scan_str(s),
            Value::Object(obj) => {
                let mut keys: Vec<&String> = obj.keys().collect();
                keys.sort();
                for key in keys {
                    if let Some(alert) = self.scan_arguments(&obj[key]) {
                        return Some(alert);
                    }
                }
                None
            }
            Value::Array(arr) => {
                for value in arr {
                    if let Some(alert) = self.scan_arguments(value) {
                        return Some(alert);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn scan_str(&self, input: &str) -> Option<DestructiveAlert> {
        for pattern in get_argument_patterns() {
            if let Some(m) = pattern.find(input) {
                return Some(DestructiveAlert {
                    matched: m.as_str().to_owned(),
                    location: DestructiveLocation::Argument,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_delete_in_tool_name() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        let alert = detector
            .scan_tool_name("delete_file")
            .expect("should match");
        assert_eq!(alert.location, DestructiveLocation::ToolName);
        assert_eq!(alert.matched, "delete");
    }

    #[test]
    fn detects_drop_and_force_in_tool_name() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        assert!(detector.scan_tool_name("drop_database").is_some());
        assert!(detector.scan_tool_name("force_push").is_some());
    }

    #[test]
    fn allows_clean_tool_names() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        assert!(detector.scan_tool_name("search_repositories").is_none());
        assert!(detector.scan_tool_name("get_file_contents").is_none());
        assert!(detector.scan_tool_name("write_file").is_none());
    }

    /// Regression: substring matching (the original implementation)
    /// falsely matched these — the keyword appears inside an unrelated
    /// word, not as its own `_`-separated token. `airdrop_tokens` is a
    /// real Web3/NFT MCP tool shape, not a hypothetical.
    #[test]
    fn does_not_match_keyword_embedded_in_unrelated_word() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        assert!(detector.scan_tool_name("enforce_policy").is_none());
        assert!(detector.scan_tool_name("airdrop_tokens").is_none());
        assert!(detector.scan_tool_name("undelete_file").is_none());
        assert!(detector.scan_tool_name("workforce_report").is_none());
        assert!(detector.scan_tool_name("truncate_string").is_none());
        assert!(detector.scan_tool_name("truncate_text").is_none());
    }

    #[test]
    fn detects_rm_rf_in_arguments() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        let args = serde_json::json!({ "command": "rm -rf /tmp/data" });
        let alert = detector.scan_arguments(&args).expect("should match");
        assert_eq!(alert.location, DestructiveLocation::Argument);
    }

    #[test]
    fn detects_drop_table_in_nested_arguments() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        let args = serde_json::json!({ "query": { "sql": "DROP TABLE users" } });
        assert!(detector.scan_arguments(&args).is_some());
    }

    #[test]
    fn allows_clean_arguments() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        let args = serde_json::json!({ "query": "SELECT * FROM users" });
        assert!(detector.scan_arguments(&args).is_none());
    }

    /// Regression: `--force\b` (the original pattern) also matched
    /// these, since `\b` sits at the hyphen boundary regardless of what
    /// follows it.
    #[test]
    fn does_not_match_force_as_prefix_of_another_flag() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        let args = serde_json::json!({ "cmd": "docker compose up --force-recreate" });
        assert!(detector.scan_arguments(&args).is_none());
    }

    #[test]
    fn detects_bare_force_flag_in_arguments() {
        let detector = DestructivePatternDetector::new(DestructiveMode::Block);
        let args = serde_json::json!({ "cmd": "git push --force" });
        assert!(detector.scan_arguments(&args).is_some());
    }

    #[test]
    fn mode_is_stored_and_returned() {
        assert_eq!(
            DestructivePatternDetector::new(DestructiveMode::Warn).mode(),
            DestructiveMode::Warn
        );
        assert_eq!(
            DestructivePatternDetector::new(DestructiveMode::Block).mode(),
            DestructiveMode::Block
        );
    }
}
