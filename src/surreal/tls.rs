//! rustls client configuration for `wss://` connections.
//!
//! Trusted roots are the connection's CA file when one is set, and the
//! bundled Mozilla roots (`webpki-roots`) otherwise, never both: like
//! PostgreSQL's `sslrootcert` and MySQL's `--ssl-ca`, the CA file is the only
//! trust anchor. The SSL mode picks how much of the server certificate is
//! checked; see [`TlsVerify`].

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
};

use crate::models::{TlsSettings, TlsVerify};

/// Build the client configuration, or explain why it cannot be built.
pub fn client_config(settings: &TlsSettings) -> Result<ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = Arc::new(root_store(settings.ca_file.as_deref())?);
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = match settings.verify {
        TlsVerify::Full => builder.with_root_certificates(roots).with_no_client_auth(),
        TlsVerify::CaOnly => {
            let inner = WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|e| e.to_string())?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyHostName { inner }))
                .with_no_client_auth()
        }
        TlsVerify::None => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateCheck { provider }))
            .with_no_client_auth(),
    };
    Ok(config)
}

/// The CA file's certificates alone, or the Mozilla roots without one.
///
/// Trusting the public roots next to a private CA would let anyone with a
/// publicly valid certificate pass: for any name under Verify CA (which does
/// not check the name), and for the server's own name under Verify Identity.
fn root_store(ca_file: Option<&str>) -> Result<RootCertStore, String> {
    let Some(path) = ca_file else {
        return Ok(RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        });
    };
    let mut roots = RootCertStore::empty();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
        .map_err(|e| format!("Could not read the CA certificate file {path}: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Could not parse the CA certificate file {path}: {e}"))?;
    if certs.is_empty() {
        return Err(format!(
            "The CA certificate file {path} contains no PEM certificate."
        ));
    }
    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| format!("Invalid CA certificate in {path}: {e}"))?;
    }
    Ok(roots)
}

/// "Verify CA": the chain must lead to a trusted root; the host name is not
/// checked.
#[derive(Debug)]
struct AnyHostName {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for AnyHostName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// "Required": encrypted, but any certificate is accepted. The handshake
/// signatures are still verified, so the session key belongs to whoever
/// presented the certificate.
#[derive(Debug)]
struct NoCertificateCheck {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoCertificateCheck {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_every_mode_with_bundled_roots() {
        for verify in [TlsVerify::Full, TlsVerify::CaOnly, TlsVerify::None] {
            client_config(&TlsSettings {
                verify,
                ca_file: None,
            })
            .unwrap();
        }
    }

    #[test]
    fn reports_unreadable_or_empty_ca_files() {
        let missing = client_config(&TlsSettings {
            verify: TlsVerify::Full,
            ca_file: Some("/nonexistent/ca.pem".into()),
        })
        .unwrap_err();
        assert!(missing.contains("Could not read the CA certificate file /nonexistent/ca.pem"));

        let dir = std::env::temp_dir().join(format!("surrealdb-plugin-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        let err = client_config(&TlsSettings {
            verify: TlsVerify::Full,
            ca_file: Some(empty.to_string_lossy().into()),
        })
        .unwrap_err();
        assert!(err.contains("contains no PEM certificate"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A self-signed P-256 CA, valid until 2126, used only to fill a CA file.
    const TEST_CA: &str = "\
-----BEGIN CERTIFICATE-----\n\
MIIBnDCCAUKgAwIBAgIUKRHsH3oOhvXAaR0X39FDJuniQZIwCgYIKoZIzj0EAwIw\n\
KzEpMCcGA1UEAwwgVGFidWxhcmlzIFN1cnJlYWxEQiB1bml0IHRlc3QgQ0EwIBcN\n\
MjYxMDI0MDU0MjI5WhgPMjEyNjA5MzAwNTQyMjlaMCsxKTAnBgNVBAMMIFRhYnVs\n\
YXJpcyBTdXJyZWFsREIgdW5pdCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0D\n\
AQcDQgAEIKycUAh0YYGosDqQYxq5iwac7TMZjHR0vYMWEQGY3N4BF8+qgx3zqscn\n\
I94ATzZsTQyNBLPhnUFWLFrMF9IXE6NCMEAwHQYDVR0OBBYEFEmNY4pvEgolm+5a\n\
EAPmzDjW+1w1MA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgIEMAoGCCqG\n\
SM49BAMCA0gAMEUCIQCSDuPYmXvH3MC3eHyKrXSOnSdjHec/pTGHx/Q+BS2dOwIg\n\
Pt20CXXgdr8xgxSQXMA/VEma166Gr0V4CbACqJizTSQ=\n\
-----END CERTIFICATE-----\n\
";

    #[test]
    fn a_ca_file_replaces_the_public_roots() {
        assert_eq!(
            root_store(None).unwrap().len(),
            webpki_roots::TLS_SERVER_ROOTS.len()
        );
        let dir =
            std::env::temp_dir().join(format!("surrealdb-plugin-tls-roots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ca.pem");
        std::fs::write(&path, TEST_CA).unwrap();
        let roots = root_store(Some(&path.to_string_lossy())).unwrap();
        assert_eq!(roots.len(), 1, "only the CA file is trusted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
