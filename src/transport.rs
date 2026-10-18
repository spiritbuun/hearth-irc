use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector, rustls};

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub type BoxStream = Box<dyn Stream>;

pub fn password(path: Option<&Path>) -> io::Result<Option<String>> {
    path.map(|p| {
        let value = std::fs::read_to_string(p)?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "password file must contain one nonempty line, at most 256 bytes",
            ));
        }
        Ok(value.to_owned())
    })
    .transpose()
}

pub fn tls_acceptor(cert: &Path, key: &Path) -> io::Result<TlsAcceptor> {
    let certificates = rustls_pemfile::certs(&mut BufReader::new(File::open(cert)?))
        .collect::<io::Result<Vec<_>>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(key)?))?
        .ok_or_else(|| io::Error::other("no TLS private key"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(io::Error::other)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub fn tls_connector(ca: Option<&Path>) -> io::Result<TlsConnector> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca {
        for cert in rustls_pemfile::certs(&mut BufReader::new(File::open(path)?)) {
            roots.add(cert?).map_err(io::Error::other)?;
        }
    }
    Ok(TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )))
}
