//! Certificate status per domain, read from what Caddy actually serves: a TLS handshake
//! to the node's own Caddy (`127.0.0.1:443`) with the domain as SNI. The served
//! certificate is inspected, not trusted; nothing is sent after the handshake.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use der::Decode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;
use x509_cert::Certificate;

use crate::executor::{CertState, CertStatus};

/// Accepts any certificate chain (we only read it) while still checking handshake
/// signatures with the provider's algorithms.
#[derive(Debug)]
struct Inspect(rustls::crypto::WebPkiSupportedAlgorithms);

impl ServerCertVerifier for Inspect {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

fn connector() -> TlsConnector {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let algorithms = provider.signature_verification_algorithms;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map(|b| {
            b.dangerous()
                .with_custom_certificate_verifier(Arc::new(Inspect(algorithms)))
                .with_no_client_auth()
        });
    match config {
        Ok(config) => TlsConnector::from(Arc::new(config)),
        // Safe defaults always exist for the bundled provider; fall back to an empty
        // config that fails every handshake rather than panicking.
        Err(_) => TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Inspect(algorithms)))
                .with_no_client_auth(),
        )),
    }
}

/// Probe one hostname against the TLS endpoint at `addr`.
pub async fn probe(addr: SocketAddr, hostname: &str, timeout: Duration) -> CertStatus {
    let status = |state, detail: &str| CertStatus {
        hostname: hostname.to_string(),
        state,
        issuer: None,
        not_after: None,
        detail: Some(detail.to_string()),
    };
    let Ok(server_name) = ServerName::try_from(hostname.to_string()) else {
        return status(CertState::Error, "not a valid TLS server name");
    };
    let handshake = async {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| (true, e.to_string()))?;
        connector()
            .connect(server_name, tcp)
            .await
            .map_err(|e| (false, e.to_string()))
    };
    let tls = match tokio::time::timeout(timeout, handshake).await {
        Err(_) => return status(CertState::Error, "the proxy did not answer in time"),
        Ok(Err((true, e))) => {
            return status(
                CertState::Error,
                &format!("proxy unreachable on :443 ({e})"),
            );
        }
        // Caddy aborts the handshake when it holds no certificate for the name.
        Ok(Err((false, _))) => {
            return status(
                CertState::Pending,
                "check that the domain's DNS points at this server",
            );
        }
        Ok(Ok(tls)) => tls,
    };
    let Some(leaf) = tls.get_ref().1.peer_certificates().and_then(|c| c.first()) else {
        return status(CertState::Pending, "the proxy presented no certificate");
    };
    classify(hostname, leaf.as_ref(), Utc::now())
}

fn classify(hostname: &str, der: &[u8], now: DateTime<Utc>) -> CertStatus {
    let Ok(cert) = Certificate::from_der(der) else {
        return CertStatus {
            hostname: hostname.to_string(),
            state: CertState::Error,
            issuer: None,
            not_after: None,
            detail: Some("unreadable certificate".into()),
        };
    };
    let tbs = cert.tbs_certificate();
    let issuer = issuer_label(&tbs.issuer().to_string());
    let secs = tbs.validity().not_after.to_unix_duration().as_secs();
    let not_after = DateTime::<Utc>::from_timestamp(i64::try_from(secs).unwrap_or(i64::MAX), 0);
    let state = if not_after.is_some_and(|t| t <= now) {
        CertState::Expired
    } else if issuer.contains("Caddy Local Authority") {
        CertState::LocalCa
    } else {
        CertState::Issued
    };
    CertStatus {
        hostname: hostname.to_string(),
        state,
        issuer: Some(issuer),
        not_after,
        detail: None,
    }
}

/// "CN=R11,O=Let's Encrypt,C=US" -> "Let's Encrypt (R11)"; falls back to the raw name.
fn issuer_label(name: &str) -> String {
    let field = |key: &str| {
        name.split(',')
            .filter_map(|p| p.trim().split_once('='))
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.to_string())
    };
    match (field("O"), field("CN")) {
        (Some(o), Some(cn)) if o != cn => format!("{o} ({cn})"),
        (Some(o), _) => o,
        (None, Some(cn)) => cn,
        (None, None) => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, DnType, KeyPair};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::net::TcpListener;
    use tokio_rustls::rustls;

    /// A TLS server on 127.0.0.1 serving a self-signed certificate.
    async fn tls_server(org: &str, expired: bool) -> SocketAddr {
        let mut params = CertificateParams::new(vec!["app.test".to_string()]).unwrap();
        params
            .distinguished_name
            .push(DnType::OrganizationName, org);
        params
            .distinguished_name
            .push(DnType::CommonName, "Test R1");
        if expired {
            params.not_before = rcgen::date_time_ymd(2020, 1, 1);
            params.not_after = rcgen::date_time_ymd(2021, 1, 1);
        }
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(tcp).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn reads_issuer_and_expiry_from_served_certificate() {
        let addr = tls_server("Let's Encrypt", false).await;
        let s = probe(addr, "app.test", Duration::from_secs(5)).await;
        assert_eq!(s.state, CertState::Issued, "{s:?}");
        assert_eq!(s.issuer.as_deref(), Some("Let's Encrypt (Test R1)"));
        assert!(s.not_after.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn recognises_local_ca_and_expired_certificates() {
        let local = tls_server("Caddy Local Authority", false).await;
        assert_eq!(
            probe(local, "app.test", Duration::from_secs(5)).await.state,
            CertState::LocalCa
        );
        let expired = tls_server("Let's Encrypt", true).await;
        assert_eq!(
            probe(expired, "app.test", Duration::from_secs(5))
                .await
                .state,
            CertState::Expired
        );
    }

    #[tokio::test]
    async fn handshake_failures_are_pending_and_closed_ports_are_errors() {
        // A listener that closes immediately: the handshake fails as when Caddy holds
        // no certificate for the name.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                drop(tcp);
            }
        });
        let s = probe(addr, "app.test", Duration::from_secs(5)).await;
        assert_eq!(s.state, CertState::Pending, "{s:?}");

        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        assert_eq!(
            probe(closed, "app.test", Duration::from_secs(2))
                .await
                .state,
            CertState::Error
        );
    }

    #[test]
    fn issuer_labels() {
        assert_eq!(
            issuer_label("CN=R11,O=Let's Encrypt,C=US"),
            "Let's Encrypt (R11)"
        );
        assert_eq!(
            issuer_label("CN=Caddy Local Authority - ECC Intermediate"),
            "Caddy Local Authority - ECC Intermediate"
        );
    }
}
