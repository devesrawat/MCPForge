//! Tool allow/deny globs (RBAC-style policy at config time).

use crate::config::validation::validate_server_name;
use crate::config::{ServerConfig, ValidationError};
use globset::{Glob, GlobMatcher};

#[derive(Debug)]
pub struct RbacPolicy {
    allow_matchers: Vec<GlobMatcher>,
    deny_matchers: Vec<GlobMatcher>,
}

impl RbacPolicy {
    pub fn from_server_config(config: &ServerConfig) -> Result<Self, ValidationError> {
        let allow_matchers = config
            .allowed_tools
            .iter()
            .map(|p| {
                Glob::new(p)
                    .map_err(|e| {
                        ValidationError(format!("invalid allow_tools glob '{}': {}", p, e))
                    })
                    .map(|g| g.compile_matcher())
            })
            .collect::<Result<Vec<_>, _>>()?;

        let deny_matchers = config
            .deny_tools
            .iter()
            .map(|p| {
                Glob::new(p)
                    .map_err(|e| ValidationError(format!("invalid deny_tools glob '{}': {}", p, e)))
                    .map(|g| g.compile_matcher())
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            allow_matchers,
            deny_matchers,
        })
    }

    pub fn is_allowed(&self, tool_name: &str) -> bool {
        if self.deny_matchers.iter().any(|m| m.is_match(tool_name)) {
            return false;
        }
        if self.allow_matchers.is_empty() {
            return true;
        }
        self.allow_matchers.iter().any(|m| m.is_match(tool_name))
    }
}

/// Validate server names and glob patterns for every server.
pub fn validate_all_servers(
    servers: &std::collections::HashMap<String, ServerConfig>,
) -> Result<(), ValidationError> {
    for (name, cfg) in servers {
        validate_server_name(name)
            .map_err(|e| ValidationError(format!("server '{}': {}", name, e)))?;
        RbacPolicy::from_server_config(cfg)
            .map_err(|e| ValidationError(format!("server '{}': {}", name, e)))?;
    }
    Ok(())
}
