use std::prelude::v1::*;

use crate::{HttpConnError, HttpConnServer, HttpRequestReader};
use core::ops::BitOrAssign;
use rustls::ServerConfig;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
// use crate::prometheus::CollectorRegistry;
// use crate::websocket_stream::{WsDataType, WsError, WsStreamServer};
// use embedded_websocket as ws;

#[derive(Debug, Default)]
pub struct HttpServerConfig {
    pub listen_addr: String,
    pub tls_cert: Vec<u8>,
    pub tls_key: Vec<u8>,
    pub max_body_length: Option<usize>,
}

pub struct HttpServerContext {
    pub conn_id: usize,
    pub is_close: bool,
    pub peer_addr: Option<SocketAddr>,
}

pub trait HttpServerHandler {
    fn on_new_http_request(&mut self, ctx: &mut HttpServerContext, req: HttpRequestReader);
    fn on_tick(&mut self, http_conns: &mut HttpServerConns) -> TickResult;
}

pub struct HttpServerConns {
    conn_seq: usize,
    conns: BTreeMap<usize, HttpConnServer>,
}

impl HttpServerConns {
    pub fn new() -> Self {
        Self {
            conn_seq: 0,
            conns: Default::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.conns.len()
    }

    pub fn add_conn(&mut self, conn: HttpConnServer) {
        self.conns.insert(self.conn_seq, conn);
        self.conn_seq += 1;
    }

    pub fn remove_conn(&mut self, conn_id: usize) -> Option<HttpConnServer> {
        self.conns.remove(&conn_id)
    }

    pub fn write_to(&mut self, conn_id: usize, data: &[u8]) -> Result<(), HttpConnError> {
        match self.conns.get_mut(&conn_id) {
            Some(conn) => conn.write_response(data),
            None => Ok(()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TickResult {
    Idle,
    Busy,
    Error,
}

impl TickResult {
    pub fn to_busy(&mut self) {
        self.bitor_assign(TickResult::Busy);
    }

    pub fn to_error(&mut self) {
        *self = TickResult::Error;
    }
}

impl std::ops::BitOrAssign<TickResult> for TickResult {
    fn bitor_assign(&mut self, rhs: TickResult) {
        *self = match self {
            TickResult::Idle => rhs,
            TickResult::Busy => {
                if let TickResult::Idle = rhs {
                    TickResult::Busy
                } else {
                    rhs
                }
            }
            TickResult::Error => TickResult::Error,
        }
    }
}

pub struct HttpServer<H: HttpServerHandler> {
    cfg: HttpServerConfig,
    listener: TcpListener,
    tls_cfg: Option<Arc<ServerConfig>>,
    conns: HttpServerConns,
    handler: H,
}

impl<H: HttpServerHandler> HttpServer<H> {
    pub fn new(cfg: HttpServerConfig, handler: H) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind(&cfg.listen_addr).map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!("listen on [{}] fail: {}", cfg.listen_addr, err.to_string()),
            )
        })?;
        listener.set_nonblocking(true)?;
        let mut tls_cfg = None;
        if cfg.tls_cert.len() > 0 || cfg.tls_key.len() > 0 {
            let mut config = rustls::ServerConfig::new(rustls::NoClientAuth::new());
            let certs = rustls::internal::pemfile::certs(&mut cfg.tls_cert.as_slice()).unwrap();
            let keys =
                rustls::internal::pemfile::rsa_private_keys(&mut cfg.tls_key.as_slice()).unwrap();
            config.set_single_cert(certs, keys[0].clone()).unwrap();
            tls_cfg = Some(Arc::new(config))
        }
        Ok(Self {
            cfg,
            tls_cfg,
            listener,
            conns: HttpServerConns::new(),
            handler,
        })
    }

    fn tick_accept(&mut self) -> std::io::Result<usize> {
        // let _trace = crate::trace::Slowlog::new_ms("accept conns", 100);
        let mut accpeted = 0;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let conn = HttpConnServer::accept(
                        stream,
                        self.tls_cfg.clone(),
                        self.cfg.max_body_length,
                    )?;
                    self.conns.add_conn(conn);
                    accpeted += 1;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    break;
                }
                Err(err) => return Err(err), // interrupted?
            }
        }
        Ok(accpeted)
    }

    fn tick_read(&mut self) -> TickResult {
        // let _trace = crate::trace::Slowlog::new_ms("accept http requests", 100);
        let mut tick = TickResult::Idle;
        let mut remove_conn = Vec::new();
        for (conn_id, conn) in &mut self.conns.conns {
            match conn.read_request() {
                Ok(Some(http_req)) => {
                    let mut ctx = HttpServerContext {
                        conn_id: *conn_id,
                        is_close: false,
                        peer_addr: conn.peer_addr().ok(),
                    };
                    self.handler.on_new_http_request(&mut ctx, http_req);
                    if ctx.is_close {
                        conn.try_close();
                        // remove_conn.push(*conn_id);
                    }
                    tick.to_busy();
                }
                Ok(None) => {}
                Err(_err) => {
                    // glog::error!("[conn:{}] err: {:?}", conn_id, err);
                    remove_conn.push(*conn_id);
                    tick.to_busy();
                }
            };
        }
        for conn_id in remove_conn {
            self.conns.remove_conn(conn_id);
            // glog::info!("remove conn: {}", conn_id);
        }
        tick
    }

    pub fn tick(&mut self) -> TickResult {
        // let _trace = crate::trace::Slowlog::new_ms("HttpServer.tick()", 100);
        let mut tick = match self.tick_accept() {
            Ok(0) => TickResult::Idle,
            Ok(_) => TickResult::Busy,
            Err(_) => TickResult::Error,
        };
        tick |= self.tick_read();
        tick |= self.handler.on_tick(&mut self.conns);
        tick
    }

    pub fn handler(&mut self) -> &mut H {
        &mut self.handler
    }
}
