use forge_core::config::secret::{DefaultSecretResolver, SecretRef};
use secrecy::SecretString;

#[tokio::test]
async fn resolve_env_secret() {
    std::env::set_var("MCP_FORGE_TEST_VAL", "super-secret");

    let resolver = DefaultSecretResolver;
    let secret_ref = SecretRef::Env("MCP_FORGE_TEST_VAL".to_owned());

    let secret: SecretString = resolver
        .resolve("test-server", &secret_ref)
        .await
        .expect("env secret resolves");

    assert_eq!(secret.expose_secret(), "super-secret");
}
