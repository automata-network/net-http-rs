use std::prelude::v1::*;

use crate::{
    HttpRequestReader, HttpServer, HttpServerConfig, HttpServerConns, HttpServerContext,
    HttpServerHandler, TickResult, WsDataType, WsError, WsStreamServer,
};
use base::trace::Alive;
use embedded_websocket as ws;
use std::collections::BTreeMap;

pub struct HttpWsServerContext<'a> {
    pub conn_id: usize,
    pub path: &'a str,
    pub is_close: bool,
}

pub trait HttpWsServerHandler {
    fn on_new_http_request(&mut self, ctx: &mut HttpServerContext, req: HttpRequestReader);
    fn on_new_ws_conn(&mut self, ctx: &mut HttpWsServerContext);
    fn on_new_ws_request(&mut self, ctx: &mut HttpWsServerContext, ty: WsDataType, data: Vec<u8>);
    fn on_close_ws_conn(&mut self, conn_id: usize);
    fn on_tick(
        &mut self,
        http_conns: &mut HttpServerConns,
        ws_conns: &mut WsServerConns,
    ) -> TickResult;
}

/// Supports both http(s) and ws(s)
pub struct HttpWsServer<H: HttpWsServerHandler> {
    server: HttpServer<WsHandler<H>>,
}

pub struct HttpWsServerConfig {
    pub listen_addr: String,
    pub tls_cert: Vec<u8>,
    pub tls_key: Vec<u8>,
    pub frame_size: usize,
    pub http_max_body_length: Option<usize>,
}

impl<H: HttpWsServerHandler> HttpWsServer<H> {
    pub fn new(cfg: HttpWsServerConfig, handler: H) -> std::io::Result<Self> {
        let handler = WsHandler {
            pending_conn_ids: Vec::new(),
            frame_size: cfg.frame_size,
            conns: WsServerConns::new(),
            alive: Alive::new(),
            handler,
        };
        let server = HttpServer::new(
            HttpServerConfig {
                max_body_length: cfg.http_max_body_length,
                listen_addr: cfg.listen_addr.clone(),
                tls_cert: cfg.tls_cert.clone(),
                tls_key: cfg.tls_key.clone(),
            },
            handler,
        )?;
        Ok(Self { server })
    }

    pub fn tick(&mut self) -> TickResult {
        self.server.tick()
    }

    pub fn handler(&mut self) -> &mut H {
        self.server.handler().handler()
    }
}

pub struct WsServerConns {
    conns: BTreeMap<usize, WsStreamServer>,
}

impl WsServerConns {
    pub fn new() -> Self {
        Self {
            conns: BTreeMap::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.conns.len()
    }

    pub fn remove(&mut self, id: usize) -> Option<WsStreamServer> {
        self.conns.remove(&id)
    }

    pub fn get_mut(&mut self, id: usize) -> Option<&mut WsStreamServer> {
        self.conns.get_mut(&id)
    }
}

struct WsHandler<H: HttpWsServerHandler> {
    pending_conn_ids: Vec<(usize, String, ws::WebSocketContext)>,
    frame_size: usize,
    conns: WsServerConns,
    alive: Alive,
    handler: H,
}

impl<H: HttpWsServerHandler> HttpServerHandler for WsHandler<H> {
    fn on_new_http_request(&mut self, ctx: &mut HttpServerContext, mut req: HttpRequestReader) {
        // glog::info!("on new http request");
        // TODO: can we have a fast path?
        match crate::get_websocket_ctx(&mut req) {
            Ok(Some(wsctx)) => {
                self.pending_conn_ids
                    .push((ctx.conn_id, req.path().to_owned(), wsctx));
            }
            Ok(None) => {
                self.handler.on_new_http_request(ctx, req);
            }
            Err(err) => {
                glog::error!("err: {:?}", err);
                ctx.is_close = true;
            }
        }
    }
    fn on_tick(&mut self, conns: &mut HttpServerConns) -> TickResult {
        // let _trace = crate::trace::Slowlog::new_ms("HttpWsServerHandler.tick()", 100);
        let mut tick = self.transform_conns(conns);
        tick |= self.tick_read();
        tick |= self.handler.on_tick(conns, &mut self.conns);
        tick
    }
}

impl<H: HttpWsServerHandler> WsHandler<H> {
    fn transform_conns(&mut self, conns: &mut HttpServerConns) -> TickResult {
        // let _trace = crate::trace::Slowlog::new_ms("HttpWsServerHandler.transform_conns()", 100);
        let mut tick = TickResult::Idle;
        loop {
            match self.pending_conn_ids.pop() {
                Some((conn_id, path, ctx)) => match conns.remove_conn(conn_id) {
                    Some(conn) => {
                        let alive = self.alive.clone();
                        tick.to_busy();
                        match WsStreamServer::from_http_conn(
                            conn,
                            &path,
                            self.frame_size,
                            ctx,
                            alive,
                        ) {
                            Ok(ws_stream) => {
                                self.conns.conns.insert(conn_id, ws_stream);
                                let mut ctx = HttpWsServerContext {
                                    conn_id,
                                    path: &path,
                                    is_close: false,
                                };
                                self.handler.on_new_ws_conn(&mut ctx);
                                if ctx.is_close {
                                    self.conns.remove(conn_id);
                                }
                            }
                            Err(err) => {
                                glog::error!("transform to ws_stream fail: {:?}", err);
                            }
                        }
                    }
                    None => {}
                },
                None => break,
            }
        }
        tick
    }

    fn tick_read(&mut self) -> TickResult {
        let mut tick = TickResult::Idle;
        let mut remove_conn = Vec::new();
        for (conn_id, stream) in &mut self.conns.conns {
            let mut data = Vec::new();
            match stream.read(&mut data) {
                Ok(ty) => {
                    let path = stream.cfg.endpoint.path();
                    let mut ctx = HttpWsServerContext {
                        conn_id: *conn_id,
                        path,
                        is_close: false,
                    };
                    self.handler.on_new_ws_request(&mut ctx, ty, data);
                    if ctx.is_close {
                        remove_conn.push(*conn_id);
                    }
                    tick.to_busy();
                }
                Err(WsError::WouldBlock) => continue,
                Err(_) => {
                    tick.to_busy();
                    remove_conn.push(*conn_id);
                }
            }
        }
        for conn_id in remove_conn {
            // glog::info!("remove conn: {}, remain: {}", conn_id, self.conns.len());
            self.conns.remove(conn_id);
            self.handler.on_close_ws_conn(conn_id);
        }
        tick
    }

    pub fn handler(&mut self) -> &mut H {
        &mut self.handler
    }
}
