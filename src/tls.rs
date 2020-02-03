use core::task::{
    Context,
    Poll,
};

use futures::stream::Stream;
use log::*;
use rustls::internal::pemfile;
use std::{
    fs,
    io,
    path::Path,
    pin::Pin,
};

use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

pub struct HyperAcceptor<'a> {
    acceptor: Pin<Box<dyn Stream<Item = Result<TlsStream<TcpStream>, io::Error>> + 'a>>,
}

impl<'a> HyperAcceptor<'a> {
    pub fn new(
        acceptor: Pin<Box<dyn Stream<Item = Result<TlsStream<TcpStream>, io::Error>> + 'a>>,
    ) -> Self {
        HyperAcceptor { acceptor }
    }
}

impl hyper::server::accept::Accept for HyperAcceptor<'_> {
    type Conn = TlsStream<TcpStream>;
    type Error = io::Error;

    fn poll_accept(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
        Pin::new(&mut self.acceptor).poll_next(cx)
    }
}

fn error(err: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err)
}

// Load public certificate from file.
pub fn load_certs<P: AsRef<Path>>(path: P) -> io::Result<Vec<rustls::Certificate>> {
    // Open certificate file.
    let certfile = fs::File::open(&path).map_err(|e| {
        error(format!(
            "failed to open file: {}; error: {}",
            path.as_ref().display(),
            e
        ))
    })?;

    let mut reader = io::BufReader::new(certfile);

    // Load and return certificate.
    pemfile::certs(&mut reader).map_err(|_| error("failed to load certificate".into()))
}

// Load private key from file.
pub fn load_private_key<P: AsRef<Path>>(path: P) -> io::Result<rustls::PrivateKey> {
    // Open keyfile.
    let keyfile = fs::File::open(&path).map_err(|e| {
        error(format!(
            "failed to open file: {}; error: {}",
            path.as_ref().display(),
            e
        ))
    })?;

    let mut reader = io::BufReader::new(keyfile);

    // Load and return a single private key.
    let keys = pemfile::rsa_private_keys(&mut reader)
        .map_err(|_| error("failed to load private key".into()))?;
    debug!("keys: {:?}", keys);
    if keys.len() != 1 {
        return Err(error("expected a single private key".into()));
    }

    Ok(keys[0].clone())
}
