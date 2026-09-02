//! GitHub App identity and installation-token handling.
//!
//! A GitHub App authenticates as itself with a JWT (signed by its private key),
//! then acts on a repository through an *installation* — either the app's own
//! identity (to discover which repo an installation applies to) or an
//! installation access token (to act with that installation's permissions).
//!
//! This module owns:
//! - [`AppCredentials`]: the app id + private key, the identity a deployment
//!   holds in config/secrets.
//! - [`AppCredentials::app_client`]: build the app-authenticated `octocrab`
//!   client.
//! - [`install_client`]: resolve the installation for a repo and return an
//!   installation-scoped client, ready to make API calls on that repo's behalf.
//!
//! The pure, testable core here is credential parsing (the PEM key must be a
//! valid RSA private key). The network calls are async and exercised
//! end-to-end only against a live GitHub; they are thin wrappers over octocrab.

use anyhow::{Context, Result};
use octocrab::Octocrab;
use octocrab::models::AppId;
use secrecy::ExposeSecret;

/// A GitHub App's identity: its numeric app id and its RSA private key.
///
/// The key is the PEM-encoded private key from the GitHub App settings page.
/// It is parsed into a signing key lazily by [`Self::encoding_key`], so an
/// invalid key fails fast at client-build time rather than being silently
/// accepted.
#[derive(Debug, Clone)]
pub struct AppCredentials {
    /// The GitHub App numeric id.
    pub app_id: u64,
    /// PEM or DER encoded RSA private key (the app's signing key).
    pub private_key_pem: String,
}

impl AppCredentials {
    /// Parse the private key into the `jsonwebtoken::EncodingKey` octocrab
    /// uses to sign the app's JWT.
    ///
    /// Returns an error if the key is not a valid RSA private key in PEM (or
    /// DER) form.
    pub fn encoding_key(&self) -> Result<jsonwebtoken::EncodingKey> {
        jsonwebtoken::EncodingKey::from_rsa_pem(self.private_key_pem.as_bytes())
            .context("invalid GitHub App RSA private key (expected a PEM-encoded RSA key)")
    }

    /// Build the app-authenticated `octocrab` client.
    ///
    /// This client acts as the GitHub App itself (not a specific repository or
    /// installation) and can be used to resolve the installation for a repo or
    /// organization, and to mint installation access tokens.
    pub fn app_client(&self) -> Result<Octocrab> {
        let key = self.encoding_key()?;
        Ok(Octocrab::builder().app(AppId(self.app_id), key).build()?)
    }
}

/// Resolve the installation that covers `owner/repo` and return an
/// installation-scoped client.
///
/// `app` must be an app-authenticated client (from
/// [`AppCredentials::app_client`]). The returned client can make API calls
/// (list open PRs, read repo state, etc.) with the installation's permissions.
///
/// # Errors
///
/// Returns an error if the repo is not installed / reachable for this app, or
/// if the app client is not app-authenticated.
pub async fn install_client(app: &Octocrab, owner: &str, repo: &str) -> Result<Octocrab> {
    let installation = app.apps().get_repository_installation(owner, repo).await?;
    Ok(app.installation(installation.id)?)
}

/// Obtain a short-lived installation access token for `owner/repo`, usable for
/// git HTTPS operations (e.g. cloning / fetching the fork).
///
/// `app` must be app-authenticated. The token is valid for ~1 hour.
pub async fn install_https_token(app: &Octocrab, owner: &str, repo: &str) -> Result<String> {
    let installation = app.apps().get_repository_installation(owner, repo).await?;
    let scoped = app.installation(installation.id)?;
    let token = scoped.installation_token().await?;
    Ok(token.expose_secret().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A generated 2048-bit RSA private key (PKCS#8, PEM). Does not correspond
    // to any real app; used only to exercise the parsing path.
    const VALID_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDE6ZxPbM1excxP\n\
/ts+MBu50Wy7tZrlgrnd0j02pS87O55JDIncflHFVlEnzxvED9YSdUP5cBorpzHj\n\
/oyMwwzucgdLz7znfBmOBHRLzyVIZDSI3e+6wS950X96PFeygNzeugSlt/5yaHAU\n\
aw2ksYwA8cC2I/L2gIaeq5jSB4lu69j0f7vZ09wlWfF0T9c7awsLmd9ZB1kXtxc1\n\
u13OhCGfwIwqQSp8bDnGEaNKHG+KRVS1uLPUvf/ppXc7S51ASs+OP6TimDK5Q/sy\n\
eDVklk0WCtY9D1CQOLlsM3Q9LjxtBE6WUIpSH3OcvgR9RVMMJD/ghv5kmzz8/g84\n\
+lt9C5FlAgMBAAECggEADNooiyCRWPW1C6WZPrpaHOs32xqT1RYOtpUxzJ7EcevG\n\
vYLVfrA5+xTSsDP63nWgJc1ElgPEmYPMpFJpbwiOEAQeSMryy/MUIepVgtsu1kZA\n\
DYzayEgyppNPbDWDDdIOWHUwt3ZGY8ZARnzkQ5MZDbi5uMmo4oDqCHIvw8mdJUGf\n\
JmEY4xoMNMipamJARSqQdLPzlGO7fobSK9b1Zr+hxi3iT1Q0pUgrAgm96abiCmNl\n\
fLcCk3f3IIlsA9pS8zz2netJnvACr2I18eP/KW+uQXJL4CR4SgsHnScDPR23aeIz\n\
muXm4EuTdPFL/cfSJcPSwnKuqdx9jeKRkBz7K+ktkQKBgQDu9FIaHfWPdVNgasGQ\n\
5dzM4qBpgLfE120ehVAPSHDyhbqq9HlzGA2kVyq8GBvPpqDg4IUgNFpRFercVKKF\n\
TL/N0AwhAikUAXKd7JNeuJFz4w39rfyCJuvMrCJ1MicSN/BAFEYyfYOkp5y8Jmc+\n\
TCCJJkp+sMPG90qKfh36E+ta6QKBgQDS9YpZseCs0EqpZItz4i2zW0CaTUuL213E\n\
8N4kBOu+JleaVlYn/zUfIZfgcFit7/HnPjwlvy3BDPh8H2f19DTdPiHT4UtDgEFS\n\
A/mK99sXyuioai+3jHEu7mk/q9ekzNDwEbwx71Vpe+wr4H/THx1jGoFglDPmwOYI\n\
iMubI/r9HQKBgDhBctbNONOWVpO7bmizhQEDVaqg8CK6aOknj4qZjmW6UBERT0pm\n\
XkfTca8oqduAKh3nHdBQIvc2Br3qevyQ7hMBKOnYfV1FXfuKB8PkBfJXgSK5BFqL\n\
2TWtTMt0jDhAzSH44/HdFNH91+t/ywyilYJUbnNXIDBGZdknCd2nNOCJAoGAZ92C\n\
x5SfpRZMnEgnrN+gVp1IGnCSEILqEQvyo1NU6mMgYJm/g6PQeMpmZ5eI4eKwfIUU\n\
whT1pwYG1b30xpD88i0kJJjZIJvmDUZtt7E+yuEZWcomQj3AgDXb1gB6hOZevMRO\n\
n1tR90SPTC8VYFICewfSyUVOpH83At6vOGwnqDUCgYEA2M64vDU1JWLH+zbjVllr\n\
zUJn4BJgiq9+Q4U8sKMSbO9sd6wBRmfzZ4LpazdiQcQ8tzjR5E2PL5KpJ957CJTn\n\
jfTuHZ8+mdrCgO2NkRbnrE0AwBFlLNsVxKVrlGcJK9u4svMjKIEHtcoj/8zZgQed\n\
17pheeFOdZKKvfPj+Aqh0bg=\n\
-----END PRIVATE KEY-----";

    fn creds(pem: &str) -> AppCredentials {
        AppCredentials {
            app_id: 12345,
            private_key_pem: pem.into(),
        }
    }

    #[test]
    fn parses_valid_rsa_private_key() {
        let key = creds(VALID_PEM).encoding_key().expect("valid key parses");
        // Parsing succeeded; the key is a concrete encoding key.
        let _ = key;
    }

    #[test]
    fn rejects_garbage_key() {
        let err = creds("definitely not a key").encoding_key().unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("invalid GitHub App RSA private key"),
            "unexpected error: {chain}"
        );
    }

    #[test]
    fn app_client_rejects_bad_key() {
        let err = creds("garbage").app_client().unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid GitHub App RSA private key"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn app_client_builds_with_valid_key() {
        // Building the app client with a valid key must succeed (no network is
        // touched at build time).
        let client = creds(VALID_PEM).app_client().expect("client builds");
        let _ = client;
    }
}
