use forge_core::config::{
    SecretResolver,
    secret::{DefaultSecretResolver, SecretRef},
};
use secrecy::{ExposeSecret, SecretString};

#[tokio::test]
async fn resolve_env_secret() {
    // SAFETY: test process is single-threaded for this block; env is restored below.
    unsafe {
        std::env::set_var("MCP_FORGE_TEST_VAL", "super-secret");
    }

    let resolver = DefaultSecretResolver;
    let secret_ref = SecretRef::Env("MCP_FORGE_TEST_VAL".to_owned());

    let secret: SecretString = resolver
        .resolve("test-server", &secret_ref)
        .await
        .expect("env secret resolves");

    assert_eq!(secret.expose_secret(), "super-secret");

    unsafe {
        std::env::remove_var("MCP_FORGE_TEST_VAL");
    }
}
