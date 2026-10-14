//! Optional TLS for the pool wire (stratum over TLS).
//!
//! The pool wire is plaintext by default. When the pool is started with a
//! PEM certificate + key it serves TLS on its bound port, and miners connect
//! with `--tls`. Client verification uses the Mozilla root store by default
//! (Let's Encrypt works), with an optional extra CA file for private CAs and
//! an explicit `insecure` switch that is for tests only.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};

/// A bidirectional byte stream: plain TCP or TLS (server or client side).
pub trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

fn load_certs(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let f = File::open(path)?;
    let mut r = BufReader::new(f);
    rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn load_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    let f = File::open(path)?;
    let mut r = BufReader::new(f);
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no private key found in PEM"))
}

/// Server config from a PEM certificate chain + private key.
pub fn server_config(cert: &Path, key: &Path) -> io::Result<Arc<ServerConfig>> {
    let certs = load_certs(cert)?;
    let key = load_key(key)?;
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Arc::new(cfg))
}

/// Client config: Mozilla roots by default, plus an optional extra CA file,
/// or `insecure` to skip verification (tests only — never a public pool).
pub fn client_config(ca: Option<&Path>, insecure: bool) -> io::Result<Arc<ClientConfig>> {
    let cfg = if insecure {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        if let Some(path) = ca {
            for cert in load_certs(path)? {
                roots
                    .add(cert)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            }
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    Ok(Arc::new(cfg))
}

/// Test-only verifier that accepts any certificate. Never use for a pool
/// that handles real value.
#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Wrap an accepted TCP stream as either plain or TLS server side.
pub fn accept(
    stream: std::net::TcpStream,
    cfg: Option<&Arc<ServerConfig>>,
) -> io::Result<Box<dyn ReadWrite>> {
    match cfg {
        Some(cfg) => {
            let conn = rustls::ServerConnection::new(cfg.clone())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            Ok(Box::new(rustls::StreamOwned::new(conn, stream)))
        }
        None => Ok(Box::new(stream)),
    }
}

/// Wrap a connected TCP stream as either plain or TLS client side.
pub fn connect(
    sock: std::net::TcpStream,
    server_name: &str,
    cfg: Option<&Arc<ClientConfig>>,
) -> io::Result<Box<dyn ReadWrite>> {
    match cfg {
        Some(cfg) => {
            let name = ServerName::try_from(server_name.to_string())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let conn = rustls::ClientConnection::new(cfg.clone(), name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            Ok(Box::new(rustls::StreamOwned::new(conn, sock)))
        }
        None => Ok(Box::new(sock)),
    }
}
