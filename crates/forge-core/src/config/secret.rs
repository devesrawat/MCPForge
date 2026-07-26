use anyhow::{Result, anyhow};
use async_trait::async_trait;
use keyring::Entry;
use secrecy::SecretString;
use serde::de::{Deserialize, Deserializer};
use serde::{Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    Env(String),
    Keychain(String),
    Literal(String),
}

impl Serialize for SecretRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            SecretRef::Env(name) => format!("env:{name}"),
            SecretRef::Keychain(name) => format!("keychain:{name}"),
            // Never write the plaintext literal to disk.
            SecretRef::Literal(_) => "literal:[redacted-on-save]".to_owned(),
        };
        serializer.serialize_str(&s)
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if let Some(name) = raw.strip_prefix("env:") {
            Ok(SecretRef::Env(name.to_owned()))
        } else if let Some(name) = raw.strip_prefix("keychain:") {
            Ok(SecretRef::Keychain(name.to_owned()))
        } else {
            Ok(SecretRef::Literal(raw))
        }
    }
}

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, service: &str, secret_ref: &SecretRef) -> Result<SecretString>;
}

/// OS keychain service name to store/read secrets under. Scoped to the
/// active `FORGE_HOME` override so secrets for different clients (each
/// run with a different `FORGE_HOME`) never collide in the OS keychain.
/// When `FORGE_HOME` is unset, returns the plain `"mcp-forge"` name used
/// by every existing install, so already-stored secrets keep working.
pub fn keychain_service_name() -> String {
    match crate::supervisor::forge_home_override() {
        Some(home) => format!("mcp-forge:{home}"),
        None => "mcp-forge".to_owned(),
    }
}

pub struct DefaultSecretResolver;

#[async_trait]
impl SecretResolver for DefaultSecretResolver {
    async fn resolve(&self, service: &str, secret_ref: &SecretRef) -> Result<SecretString> {
        match secret_ref {
            SecretRef::Env(var) => std::env::var(var)
                .map(SecretString::from)
                .map_err(|_| anyhow!("env var '{}' not set (needed by server '{}')", var, service)),
            SecretRef::Keychain(key) => {
                let keychain_service = keychain_service_name();
                let entry = Entry::new(&keychain_service, key)
                    .map_err(|e| anyhow!("invalid keychain entry '{}': {}", key, e))?;
                match entry.get_password() {
                    Ok(password) => Ok(SecretString::from(password)),
                    Err(keyring::Error::NoEntry) => Err(anyhow!(
                        "keychain entry '{}/{}' not found. Run: forge secret set {}",
                        keychain_service,
                        key,
                        key
                    )),
                    Err(keyring::Error::NoStorageAccess(_))
                    | Err(keyring::Error::PlatformFailure(_)) => Err(anyhow!(
                        "keychain unavailable on this system. Use 'env:VAR' in forge.toml instead of 'keychain:{}' (needed by server '{}')",
                        key,
                        service
                    )),
                    Err(err) => Err(anyhow!(
                        "keychain error for '{}' on service '{}': {}",
                        key,
                        service,
                        err
                    )),
                }
            }
            SecretRef::Literal(value) => Ok(SecretString::from(value.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::keychain_service_name;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn keychain_service_name_is_unscoped_by_default() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: serialized by env_lock(); FORGE_HOME is Forge-private.
        unsafe { std::env::remove_var("FORGE_HOME") };
        assert_eq!(keychain_service_name(), "mcp-forge");
    }

    #[test]
    fn keychain_service_name_is_scoped_when_forge_home_set() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: serialized by env_lock().
        unsafe { std::env::set_var("FORGE_HOME", "/tmp/client-a") };
        assert_eq!(keychain_service_name(), "mcp-forge:/tmp/client-a");
        // SAFETY: serialized by env_lock().
        unsafe { std::env::remove_var("FORGE_HOME") };
    }
}
