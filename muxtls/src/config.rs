use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rcgen::generate_simple_self_signed;
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};

use crate::error::{Error, Result};

/// Client-side TLS configuration wrapper.
#[derive(Clone)]
pub struct ClientConfig {
    pub(crate) inner: Arc<rustls::ClientConfig>,
}

/// Server-side TLS configuration wrapper.
#[derive(Clone)]
pub struct ServerConfig {
    pub(crate) inner: Arc<rustls::ServerConfig>,
}

impl ClientConfig {
    /// Builds a client config from native platform root certificates.
    pub fn with_native_roots() -> Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        let cert_result = rustls_native_certs::load_native_certs();
        for cert in cert_result.certs {
            roots.add(cert).map_err(|e| Error::Config(e.to_string()))?;
        }

        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        Ok(Self {
            inner: Arc::new(config),
        })
    }

    /// Builds a client config using platform certificate verification.
    #[cfg(feature = "platform-verifier")]
    pub fn with_platform_verifier() -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new(provider)?;
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();

        Ok(Self {
            inner: Arc::new(config),
        })
    }

    /// Builds an insecure testing-only client config that skips certificate verification.
    pub fn dangerous_insecure_no_verify_for_testing() -> Self {
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(InsecureNoVerify))
            .with_no_client_auth();

        Self {
            inner: Arc::new(config),
        }
    }

    /// Creates a client config that trusts one or more custom root certificates.
    pub fn with_custom_roots(certs: Vec<CertificateDer<'static>>) -> Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in certs {
            roots.add(cert).map_err(|e| Error::Config(e.to_string()))?;
        }

        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        Ok(Self {
            inner: Arc::new(config),
        })
    }

    pub(crate) fn server_name(server_name: &str) -> Result<ServerName<'static>> {
        ServerName::try_from(server_name.to_owned())
            .map_err(|_| Error::InvalidDnsName(server_name.to_owned()))
    }
}

impl ServerConfig {
    /// Loads a server certificate chain and private key from PEM files.
    pub fn from_pem_files(cert_path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        let mut cert_reader = BufReader::new(File::open(cert_path)?);
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::Config(e.to_string()))?;

        if certs.is_empty() {
            return Err(Error::Config(
                "no certificates found in PEM file".to_owned(),
            ));
        }

        let mut key_reader = BufReader::new(File::open(key_path)?);
        let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
            .map_err(|e| Error::Config(e.to_string()))?
            .ok_or_else(|| Error::Config("no private key found in PEM file".to_owned()))?;

        Self::from_der(certs, key)
    }

    /// Creates a server config from DER certificates and private key.
    pub fn from_der(
        certs: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self> {
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| Error::Config(e.to_string()))?;

        Ok(Self {
            inner: Arc::new(config),
        })
    }

    /// Generates a self-signed certificate for local development or tests.
    pub fn self_signed_for_localhost() -> Result<(Self, CertificateDer<'static>)> {
        let cert =
            generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
                .map_err(|e| Error::Config(e.to_string()))?;

        let cert_der: CertificateDer<'static> = cert.cert.der().clone();
        let key_der = PrivateKeyDer::try_from(cert.signing_key.serialize_der())
            .map_err(|e| Error::Config(e.to_string()))?;

        let cfg = Self::from_der(vec![cert_der.clone()], key_der)?;
        Ok((cfg, cert_der))
    }
}

#[derive(Debug)]
struct InsecureNoVerify;

impl ServerCertVerifier for InsecureNoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
