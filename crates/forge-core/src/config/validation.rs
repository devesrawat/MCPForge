use std::fmt;

#[derive(Debug)]
pub struct ValidationError(pub String);

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ValidationError {}

/// Validate a server name: `[a-zA-Z0-9_-]`, 1–64 characters.
///
/// Server names are used as path components (log file names, stop marker
/// file names), so characters outside this set risk path traversal.
pub fn validate_server_name(name: &str) -> Result<(), ValidationError> {
    if name.is_empty() {
        return Err(ValidationError("server name must not be empty".to_owned()));
    }
    if name.len() > 64 {
        return Err(ValidationError(format!(
            "server name '{}' exceeds 64 characters",
            name
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ValidationError(format!(
            "server name '{}' contains invalid characters (allowed: a-z A-Z 0-9 _ -)",
            name
        )));
    }
    Ok(())
}
