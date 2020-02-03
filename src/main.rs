use anyhow::Result;
use base64::{
    encode_config,
    STANDARD_NO_PAD,
};

use bytes::BytesMut;
use file_server::{
    built_info,
    tls::{
        self,
        HyperAcceptor,
    },
};

use futures::{
    future,
    future::Either,
    stream::{
        SelectAll,
        StreamExt,
        TryStreamExt,
    },
};

use headers::HeaderMapExt;
use hyper::{
    body::Bytes,
    header::{
        self,
        HeaderMap,
        HeaderValue,
    },
    server::accept::from_stream,
    service::{
        make_service_fn,
        service_fn,
    },
    Body,
    Method,
    Request,
    Response,
    Server,
    StatusCode,
};

use log::*;
use mime_guess::{
    self,
    mime,
};

use rustls;
use std::{
    hash::Hasher,
    io,
    io::SeekFrom,
    net::SocketAddr,
    path::{
        Path,
        PathBuf,
    },
    sync::Arc,
};

use structopt::StructOpt;
use tokio::{
    fs::File,
    runtime::Runtime,
    signal::unix::{
        signal,
        SignalKind,
    },
};

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::codec::{
    BytesCodec,
    Decoder,
    FramedRead,
};

use twox_hash::XxHash64;

static INTERNAL_SERVER_ERROR: &[u8] = b"Internal Server Error";
static NOT_FOUND: &[u8] = b"Not Found";
static NOT_IMPLEMENTED: &[u8] = b"Not Implemented";

#[derive(Debug, StructOpt)]
#[structopt(about, rename_all = "kebab-case")]
struct Opt {
    /// Sets the TCP address to listen on
    #[structopt(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    listen_addr: SocketAddr,

    /// Sets the path to PEM file with SSL certificate and RSA key
    #[structopt(long, env = "SSL_CERT")]
    ssl: Option<PathBuf>,

    /// Sets the directory path
    #[structopt(default_value = ".", parse(from_os_str))]
    dir: PathBuf,

    /// Sets the filename extension to append to requested path
    #[structopt(long)]
    ext: Option<String>,

    /// Sets the value of Cache-Control header to return
    #[structopt(long)]
    cache_control: Option<String>,

    /// Print version and exit
    #[structopt(long, short)]
    version: bool,
}

fn version() -> String {
    format!(
        "{} {} ({}, {} build, {} [{}], {})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        built_info::GIT_VERSION.unwrap_or("unknown"),
        built_info::PROFILE,
        built_info::CFG_OS,
        built_info::CFG_TARGET_ARCH,
        built_info::BUILT_TIME_UTC,
    )
}

struct ChunkCodec;

impl Decoder for ChunkCodec {
    type Item = Bytes;
    type Error = std::io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if !buf.is_empty() {
            let len = buf.len();
            Ok(Some(Bytes::from(buf.split_to(len).freeze())))
        } else {
            Ok(None)
        }
    }
}

async fn handle_request(
    req: Request<Body>,
    dir: &Path,
    ext: Option<String>,
    cc: Option<String>,
) -> Result<Response<Body>> {
    if req.method() == Method::GET || req.method() == Method::HEAD {
        let path = req.uri().path();
        if path.contains("..") {
            Ok(error(StatusCode::NOT_FOUND, NOT_FOUND.into()))
        } else {
            let mut rel_path = if path.starts_with('/') {
                ".".to_owned() + path
            } else {
                path.to_owned()
            };

            if let Some(ext) = ext {
                rel_path.push_str(&ext);
            };

            let full_path = dir.join(rel_path);
            debug!("full path: {:?}", full_path);
            serve_file(req.headers(), &full_path, cc).await
        }
    } else {
        Ok(error(StatusCode::NOT_IMPLEMENTED, NOT_IMPLEMENTED.into()))
    }
}

fn error(code: StatusCode, body: Body) -> Response<Body> {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, mime::TEXT_PLAIN.as_ref())
        .body(body)
        .unwrap()
}

async fn serve_file(
    hdrs: &HeaderMap<HeaderValue>,
    filename: &Path,
    cc: Option<String>,
) -> Result<Response<Body>> {
    if let Ok(mut file) = File::open(filename).await {
        if let Ok(metadata) = file.metadata().await {
            if !metadata.is_file() {
                return Ok(error(StatusCode::NOT_FOUND, NOT_FOUND.into()));
            }

            let mut resp = Response::builder();

            let etag = match digest(&mut file).await {
                Ok(digest) => Some(format!(r#""{}""#, digest)),
                Err(e) => {
                    debug!("digest error: {:?}", e);
                    None
                }
            };

            if let Some(ref etag) = etag {
                resp = resp.header(header::ETAG, etag);
            }

            let modified = metadata.modified();
            if let Ok(modified) = modified {
                resp.headers_mut()
                    .and_then(|h| Some(h.typed_insert(headers::LastModified::from(modified))));
            }

            if let Some(cc) = cc {
                resp = resp.header(header::CACHE_CONTROL, cc);
            }

            if let Some(etag) = etag {
                if hdrs
                    .get_all(header::IF_NONE_MATCH)
                    .iter()
                    .filter_map(|val| val.to_str().ok())
                    .flat_map(|val| val.split(','))
                    .map(|val| val.trim())
                    .filter(|val| !val.is_empty())
                    .any(|val| val == etag)
                {
                    return Ok(resp
                        .status(StatusCode::NOT_MODIFIED)
                        .body(Body::empty())
                        .unwrap());
                }
            }

            if let Ok(modified) = modified {
                if let Some::<headers::IfModifiedSince>(val) = hdrs.typed_get() {
                    if !val.is_modified(modified) {
                        return Ok(resp
                            .status(StatusCode::NOT_MODIFIED)
                            .body(Body::empty())
                            .unwrap());
                    }
                }
            }

            if let Some(mt) = mime_guess::from_path(filename).first() {
                resp = resp.header(header::CONTENT_TYPE, mt.as_ref());
            }

            resp = resp.header(header::CONTENT_LENGTH, metadata.len());

            let chunks = FramedRead::new(file, ChunkCodec);
            return Ok(resp.body(Body::wrap_stream(chunks)).unwrap());
        }

        Ok(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            INTERNAL_SERVER_ERROR.into(),
        ))
    } else {
        Ok(error(StatusCode::NOT_FOUND, NOT_FOUND.into()))
    }
}

async fn digest(mut file: &mut File) -> Result<String> {
    let pos = file.seek(SeekFrom::Current(0)).await?;

    let mut hasher = XxHash64::default();
    let chunks = FramedRead::new(&mut file, BytesCodec::new());
    let result = chunks
        .try_for_each(|chunk| async move {
            hasher.write(&chunk);
            Ok(())
        })
        .await;

    file.seek(SeekFrom::Start(pos)).await?;
    result?;

    let hash = hasher.finish().to_be_bytes();
    Ok(encode_config(&hash, STANDARD_NO_PAD))
}

fn main() -> Result<()> {
    pretty_env_logger::init();

    let opt = Opt::from_args();

    if opt.version {
        println!("version: {}", version());
        std::process::exit(0);
    }

    let dir = opt
        .dir
        .canonicalize()
        .and_then(|dir| {
            if dir.is_dir() {
                Ok(dir)
            } else {
                Err(tokio::io::Error::new(
                    tokio::io::ErrorKind::Other,
                    "not a directory",
                ))
            }
        })
        .unwrap_or_else(|e| {
            debug!("directory error: {:?}", e);
            eprintln!("invalid directory path: {}", opt.dir.to_string_lossy());
            std::process::exit(1);
        });

    let mut tls_acceptor = None;
    if let Some(ref cert_path) = opt.ssl {
        let certs = tls::load_certs(cert_path)?;
        let key = tls::load_private_key(cert_path)?;
        // Do not use client certificate authentication.
        let mut cfg = rustls::ServerConfig::new(rustls::NoClientAuth::new());
        // Select a certificate to use.
        cfg.set_single_cert(certs, key)?;
        // Configure ALPN to accept HTTP/2, HTTP/1.1 in that order.
        cfg.set_protocols(&[b"h2".to_vec(), b"http/1.1".to_vec()]);
        tls_acceptor = Some(TlsAcceptor::from(Arc::new(cfg)));
    }

    info!("{}", version());

    let shutdown_signal = async {
        let mut signals = SelectAll::new();
        signals.push(
            signal(SignalKind::interrupt()).expect("failed to register the interrupt signal"),
        );
        signals.push(signal(SignalKind::quit()).expect("failed to register the quit signal"));
        signals.push(
            signal(SignalKind::terminate()).expect("failed to register the terminate signal"),
        );
        // ignore SIGPIPE
        let _ = signal(SignalKind::pipe()).expect("failed to register the pipe signal");

        signals.select_next_some().await
    };

    let mut rt = Runtime::new()?;

    rt.block_on(async {
        let mut tcp = TcpListener::bind(&opt.listen_addr).await?;
        let incoming = tcp.incoming();
        let server = if tls_acceptor.is_some() {
            let incoming_tls_stream = incoming
                .then(|r| async {
                    match r {
                        Ok(s) => {
                            s.set_nodelay(true)?;
                            tls_acceptor.clone().unwrap().accept(s).await
                        }
                        Err(e) => Err(e),
                    }
                })
                .filter(|r| match r {
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        debug!("accept error: {}", e);
                        future::ready(false)
                    }

                    _ => future::ready(true),
                })
                .boxed();

            let make_service = make_service_fn(|_| {
                let dir = dir.clone();
                let ext = opt.ext.clone();
                let cc = opt.cache_control.clone();
                async {
                    Ok::<_, hyper::Error>(service_fn(move |req| {
                        let dir = dir.clone();
                        let ext = ext.clone();
                        let cc = cc.clone();
                        async move { handle_request(req, &dir, ext, cc).await }
                    }))
                }
            });

            let server = Server::builder(HyperAcceptor::new(incoming_tls_stream))
                .serve(make_service)
                .with_graceful_shutdown(shutdown_signal);

            Either::Left(server)
        } else {
            let make_service = make_service_fn(|_| {
                let dir = dir.clone();
                let ext = opt.ext.clone();
                let cc = opt.cache_control.clone();
                async {
                    Ok::<_, hyper::Error>(service_fn(move |req| {
                        let dir = dir.clone();
                        let ext = ext.clone();
                        let cc = cc.clone();
                        async move { handle_request(req, &dir, ext, cc).await }
                    }))
                }
            });

            let server = Server::builder(from_stream(incoming.map(|r| {
                if let Ok(ref s) = r {
                    s.set_nodelay(true)?;
                }

                r
            })))
            .serve(make_service)
            .with_graceful_shutdown(shutdown_signal);

            Either::Right(server)
        };

        info!("listening on {}", opt.listen_addr);
        info!("serving files from {}", dir.to_string_lossy());

        server.await.map_err(Into::into)
    })
}
