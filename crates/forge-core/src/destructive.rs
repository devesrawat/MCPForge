//! Destructive-pattern detection for tool names and arguments.
//!
//! Structurally mirrors `injection.rs`: scans for patterns that suggest
//! a tool call is about to delete, drop, force, or otherwise destroy
//! data, and can operate in warn mode (log + forward) or block mode
//! (log + reject). Unlike prompt injection, this has no "scan the
//! result" side — a destructive pattern is meaningful on what an agent
//! is about to *do* (tool name + arguments), not on what a tool
//! *returned*.

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

/// Case-insensitive substring keywords checked against tool names. Tool
/// names are snake_case identifiers (e.g. `delete_file`, `force_push`)
/// where regex `\b` word boundaries don't separate words at
/// underscores (`_` is a word character, so `\bdelete\b` would not
/// match `delete_file`) — plain substring matching is simpler and
/// correct for this shape.
const DESTRUCTIVE_NAME_KEYWORDS: &[&str] = &[
    "delete", "drop", "destroy", "purge", "truncate", "wipe", "force",
];

fn get_argument_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"(?i)\brm\s+-rf\b").unwrap(),
            Regex::new(r"(?i)\bdrop\s+table\b").unwrap(),
            Regex::new(r"(?i)\bdrop\s+database\b").unwrap(),
            Regex::new(r"(?i)\btruncate\s+table\b").unwrap(),
            Regex::new(r"(?i)--force\b").unwrap(),
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

    /// Check a tool name for destructive keywords (case-insensitive
    /// substring match).
    pub fn scan_tool_name(&self, tool_name: &str) -> Option<DestructiveAlert> {
        let lower = tool_name.to_ascii_lowercase();
        for keyword in DESTRUCTIVE_NAME_KEYWORDS {
            if lower.contains(keyword) {
                return Some(DestructiveAlert {
                    matched: (*keyword).to_owned(),
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
