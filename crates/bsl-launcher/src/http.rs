use anyhow::{Context, Result};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::Certificate;

/// Builds an HTTP client, tolerating a machine with no system trust store.
///
/// System roots stay in charge, so a corporate CA or a MITM proxy keeps working
/// wherever it is configured. The bundled Mozilla roots exist for the bare
/// system — an image without `ca-certificates` — where rustls refuses to build a
/// client at all and the launcher dies before the single network call it needs.
pub fn build_client(configure: impl Fn(ClientBuilder) -> ClientBuilder) -> Result<Client> {
    let system_roots_error = match configure(Client::builder()).build() {
        Ok(client) => return Ok(client),
        Err(err) => err,
    };

    // Bundled roots only cure a missing trust store. If the client still refuses
    // to build, the cause lies elsewhere and the original error is the honest one.
    configure(Client::builder())
        .tls_certs_only(bundled_roots()?)
        .build()
        .map_err(|_| system_roots_error)
        .context("Failed to create HTTP client")
}

fn bundled_roots() -> Result<Vec<Certificate>> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| Certificate::from_der(der).context("Failed to load bundled root certificate"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_roots_are_present_and_parsable() {
        let roots = bundled_roots().expect("bundled roots must parse");
        assert!(roots.len() > 50, "expected a full CA bundle, got {} roots", roots.len());
    }

    /// Pointing the trust store lookup at an empty directory this test owns
    /// reproduces the bare image: rustls finds no roots and reqwest refuses to
    /// build a client. The directory is created here rather than assumed absent,
    /// so no leftover bundle on the machine can quietly satisfy the control.
    #[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
    #[test]
    fn builds_client_when_system_trust_store_is_missing() {
        let empty_store = stdx::test_temp::unique_dir("bsl-launcher-no-roots");
        std::fs::create_dir_all(&empty_store).expect("failed to create the empty trust store");
        let absent_bundle = empty_store.join("ca-bundle.pem");
        assert!(!absent_bundle.exists(), "the bundle must stay absent inside our own directory");

        std::env::set_var("SSL_CERT_FILE", &absent_bundle);
        std::env::set_var("SSL_CERT_DIR", &empty_store);

        let control = Client::builder().build();
        let fallback = build_client(|builder| builder);
        let _ = std::fs::remove_dir_all(&empty_store);

        assert!(
            control.is_err(),
            "control: the system trust store must be unreachable, otherwise this test proves nothing"
        );
        fallback.expect("bundled roots must carry the client");
    }
}
