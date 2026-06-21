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

pub struct DefaultSecretResolver;

#[async_trait]
impl SecretResolver for DefaultSecretResolver {
    async fn resolve(&self, service: &str, secret_ref: &SecretRef) -> Result<SecretString> {
        match secret_ref {
            SecretRef::Env(var) => std::env::var(var)
                .map(SecretString::from)
                .map_err(|_| anyhow!("env var '{}' not set (needed by server '{}')", var, service)),
            SecretRef::Keychain(key) => {
                let entry = Entry::new("mcp-forge", key)
                    .map_err(|e| anyhow!("invalid keychain entry '{}': {}", key, e))?;
                match entry.get_password() {
                    Ok(password) => Ok(SecretString::from(password)),
                    Err(keyring::Error::NoEntry) => Err(anyhow!(
                        "keychain entry 'mcp-forge/{}' not found. Run: forge secret set {}",
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
