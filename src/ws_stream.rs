use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use std::prelude::v1::*;

use crate::{
    HttpConnClient, HttpConnError, HttpConnServer, HttpRequestReader, HttpResponse,
    TlsServerConfig, Uri,
};
use base::trace::Alive;
use bytes::{BufferVec, ExpandVec, IOError, WriteBuffer};
use crypto::{read_rand, Sha1};
use embedded_websocket as ws;
use net::AnyStream;
use rand_core::RngCore;
use rand_websocket::rngs::StdRng;
use std::io::ErrorKind;
use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub struct WsStreamConfig {
    pub endpoint: Uri,
    pub frame_size: usize,
    pub keep_alive: Option<Duration>,
    pub auto_reconnect: bool,
    pub alive: Alive,
}

pub type WsStreamClient = WsStream<StdRng, ws::Client>;
pub type WsStreamServer = WsStream<ws::EmptyRng, ws::Server>;

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum WsDataType {
    Binary,
    Text,
}

impl From<WsDataType> for ws::WebSocketSendMessageType {
    fn from(ty: WsDataType) -> Self {
        match ty {
            WsDataType::Binary => ws::WebSocketSendMessageType::Binary,
            WsDataType::Text => ws::WebSocketSendMessageType::Text,
        }
    }
}

/// Websocket streaming (TLS supported)
///
/// Features:
///  * no overhead cost for threading/channel
///  * auto reconnect
///  * auto keep alive
///  * hide the detail of websocket protocol
///
/// Interfaces:
///  * is_connected() / connect() / disconnect(): manually connection control, it will reconnect the remote in next time.
///  * shutdown(): shutdown the WsStream
///  * read(&mut Vec<u8>) / write(&[u8]): read/write data from remote
pub struct WsStream<R, T>
where
    R: RngCore,
    T: ws::WebSocketType,
{
    pub cfg: WsStreamConfig,
    connected: Option<(AnyStream, ws::WebSocket<R, T>)>,
    connection_epoch: AtomicUsize,
    client_handshake_state: WsClientHandshakeState,
    server_handshake_state: WsServerHandshakeState,
    buffer: Vec<u8>,
    read_buffer: BufferVec,
    last_msg_time: Instant,
    continuation_buffer: ExpandVec<u8>,
    wbuf: WriteBuffer,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WsError {
    /// Error in initialize a new connection
    InitError(String),
    /// The remote side is not ready
    WouldBlock,
    /// The Connection is disconnected. Retry later if auto_reconnect is enable
    ServerDisconnected { temporary: bool },
}

pub enum WsClientHandshakeState {
    None,
    WaitSend(
        BufferVec,
        ws::WebSocketKey,
        ws::WebSocketClient<StdRng>,
        HttpConnClient,
    ),
    Send(
        ws::WebSocketKey,
        ws::WebSocketClient<StdRng>,
        HttpConnClient,
    ),
}

pub enum WsServerHandshakeState {
    None,
    WaitRequest(Instant, HttpConnServer),
    SendResponse(Instant, ws::WebSocketKey, HttpConnServer, String),
}

impl WsStreamClient {
    pub fn new(cfg: WsStreamConfig) -> Result<Self, WsError> {
        let mut stream = Self::init(cfg);
        let result = stream.connect();
        match result {
            Ok(_) => {}
            Err(WsError::WouldBlock) => {}
            Err(err) => return Err(err),
        }
        Ok(stream)
    }

    /// Errors:
    ///  * [`WsError::WouldBlock`]
    ///  * [`WsError::InitError`]
    pub fn reconnect(&mut self) -> Result<(), WsError> {
        self.connected = None;
        self.connect()
    }

    /// Errors:
    ///  * [`WsError::WouldBlock`]
    ///  * [`WsError::InitError`]
    pub fn connect(&mut self) -> Result<(), WsError> {
        self.check_alive()?;
        loop {
            if self.connected.is_some() {
                return Ok(());
            }
            match &mut self.client_handshake_state {
                WsClientHandshakeState::None => {
                    let conn = match HttpConnClient::connect(self.cfg.endpoint.clone()) {
                        Ok(conn) => conn,
                        Err(err) => return Err(WsError::InitError(format!("{:?}", err))),
                    };
                    let mut parser = {
                        let mut seed = [0_u8; 32];
                        read_rand(&mut seed);

                        let rng: StdRng = rand_websocket::SeedableRng::from_seed(seed);
                        ws::WebSocketClient::new_client(rng)
                    };

                    let uri = self.cfg.endpoint.clone();
                    let mut buffer = BufferVec::new(8096);

                    // because we're in the initial state, so we didn't need to reset the state when it has a error
                    let key = Self::client_write_handshake_info(&mut buffer, &uri, &mut parser)?;

                    self.client_handshake_state =
                        WsClientHandshakeState::WaitSend(buffer, key, parser, conn);
                }
                WsClientHandshakeState::WaitSend(buffer, _, _, conn) => {
                    match conn.write_raw_request(0, buffer.read()) {
                        Ok(_) => {}
                        Err(HttpConnError::WouldBlock) => return Err(WsError::WouldBlock),
                        Err(err) => {
                            let mut state = WsClientHandshakeState::None;
                            std::mem::swap(&mut state, &mut self.client_handshake_state);
                            return Err(WsError::InitError(format!("write error: {:?}", err)));
                        }
                    };
                    buffer.rotate_left(buffer.len());
                    let mut state = WsClientHandshakeState::None;
                    std::mem::swap(&mut state, &mut self.client_handshake_state);
                    match state {
                        WsClientHandshakeState::WaitSend(_, key, parser, conn) => {
                            self.client_handshake_state =
                                WsClientHandshakeState::Send(key, parser, conn);
                        }
                        _ => unreachable!(),
                    }
                }
                WsClientHandshakeState::Send(key, parser, conn) => {
                    match Self::client_get_handshake_response(&key, parser, conn) {
                        Ok(_) => {}
                        Err(WsError::WouldBlock) => return Err(WsError::WouldBlock),
                        Err(err) => {
                            // reset the state so we can restart the handshake
                            let mut state = WsClientHandshakeState::None;
                            std::mem::swap(&mut state, &mut self.client_handshake_state);
                            return Err(err);
                        }
                    };
                    let mut state = WsClientHandshakeState::None;
                    std::mem::swap(&mut state, &mut self.client_handshake_state);
                    match state {
                        WsClientHandshakeState::Send(_, parser, conn) => {
                            let (stream, buffer) = conn.into_raw();
                            self.connected = Some((stream, parser));
                            self.read_buffer = buffer;
                            self.connection_epoch.fetch_add(1, Ordering::SeqCst);
                            return Ok(());
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
    }

    /// Errors:
    ///  * [`WsError::ServerDisconnected`]
    ///  * [`WsError::InitError`]
    fn auto_reconnect(&mut self) -> Result<(), WsError> {
        if self.connected.is_none() {
            if !self.cfg.auto_reconnect {
                if let WsClientHandshakeState::None = self.client_handshake_state {
                    return Err(WsError::ServerDisconnected { temporary: false });
                }
            }
            self.connect()?;
        }
        Ok(())
    }

    pub fn write_ty(&mut self, ty: WsDataType, buf: &[u8]) -> Result<(), WsError> {
        self.auto_reconnect()?;
        self.write_msg(ty.into(), buf)
    }

    /// Write Binary message
    ///
    /// Errors:
    ///  * [`WsError::WouldBlock`]: remote side is not ready, retry later.
    ///  * [`WsError::ServerDisconnected`]
    pub fn write(&mut self, buf: &[u8]) -> Result<(), WsError> {
        self.auto_reconnect()?;
        self.write_msg(ws::WebSocketSendMessageType::Binary, buf)
    }

    /// Read Binary/Text message
    /// It will process other type messages internal, e.g. ping/pong. In this case, it
    /// will continue the reading until a binary/text message comes.
    ///
    /// Errors:
    ///  * [`WsError::WouldBlock`]: remote side is not ready, retry later.
    ///  * [`WsError::InitError`]: error in processing reconnect staff
    ///  * [`WsError::ServerDisconnected`]: the connection is disconnected or it's shutdown, retry later.
    pub fn read(&mut self, data: &mut Vec<u8>) -> Result<WsDataType, WsError> {
        self.auto_reconnect()?;
        self.generic_read(data)
    }

    fn client_write_handshake_info(
        buffer: &mut BufferVec,
        uri: &Uri,
        parser: &mut ws::WebSocketClient<StdRng>,
    ) -> Result<ws::WebSocketKey, WsError> {
        let host = uri
            .host()
            .map_err(|err| WsError::InitError(format!("invalid host: {:?}", err)))?;
        let origin = String::from(uri.scheme()) + "://" + host;
        let opt = ws::WebSocketOptions {
            path: uri.path(),
            host,
            origin: &origin,
            sub_protocols: None,
            additional_headers: None,
        };
        if buffer.read().len() != 0 {
            return Err(WsError::InitError(
                "internal error: buffer should be empty".into(),
            ));
        }

        let (length, key) = parser
            .client_connect(&opt, buffer.write())
            .map_err(|err| WsError::InitError(format!("Handshake connect error: {:?}", err)))?;
        buffer.advance(length);
        Ok(key)
    }

    fn client_get_handshake_response(
        key: &ws::WebSocketKey,
        parser: &mut ws::WebSocketClient<StdRng>,
        conn: &mut HttpConnClient,
    ) -> Result<(), WsError> {
        match conn.read_response() {
            Ok((_, response)) => {
                read_handshake_response(&response, &key).map_err(|err| {
                    parser.state = ws::WebSocketState::Aborted;
                    WsError::InitError(format!("Handshake accept error: {:?}", err))
                })?;
                parser.state = ws::WebSocketState::Open;
                Ok(())
            }
            Err(HttpConnError::WouldBlock) => return Err(WsError::WouldBlock),
            Err(_) => return Err(WsError::ServerDisconnected { temporary: false }),
        }
    }
}

impl WsStreamServer {
    fn server_handshake(
        &mut self,
        mut conn: Option<HttpConnServer>,
        timeout: Option<Duration>,
    ) -> Result<(), WsError> {
        loop {
            if self.is_connected() {
                return Ok(());
            }
            match &mut self.server_handshake_state {
                WsServerHandshakeState::None => {
                    let start = Instant::now();
                    let conn = match conn.take() {
                        Some(conn) => conn,
                        None => return Err(WsError::ServerDisconnected { temporary: false }),
                    };
                    self.server_handshake_state = WsServerHandshakeState::WaitRequest(start, conn);
                }
                WsServerHandshakeState::WaitRequest(start, conn) => {
                    if let Some(timeout) = timeout {
                        if start.elapsed() > timeout {
                            return Err(WsError::InitError(format!("timeout")));
                        }
                    }
                    match conn.read_request() {
                        Ok(Some(mut req)) => {
                            let ctx = get_websocket_ctx(&mut req).map_err(|err| {
                                WsError::InitError(format!(
                                    "receive handshake header fail: {:?}",
                                    err
                                ))
                            })?;
                            let ctx = ctx.ok_or_else(|| {
                                WsError::InitError(
                                    "WebSocket Protocol Error: expect a websocket context".into(),
                                )
                            })?;
                            let path = req.path().to_owned();
                            let mut state = WsServerHandshakeState::None;
                            std::mem::swap(&mut state, &mut self.server_handshake_state);
                            match state {
                                WsServerHandshakeState::WaitRequest(instant, conn) => {
                                    self.server_handshake_state =
                                        WsServerHandshakeState::SendResponse(
                                            instant,
                                            ctx.sec_websocket_key,
                                            conn,
                                            path,
                                        );
                                    continue;
                                }
                                _ => unreachable!(),
                            }
                        }
                        Ok(None) => return Err(WsError::WouldBlock),
                        Err(err) => {
                            return Err(WsError::InitError(format!("err: {:?}", err)));
                        }
                    }
                }
                WsServerHandshakeState::SendResponse(start, key, conn, path) => {
                    if let Some(timeout) = timeout {
                        if start.elapsed() > timeout {
                            return Err(WsError::InitError(format!("timeout")));
                        }
                    }
                    let endpoint = format!("ws://localhost{}", path);
                    let endpoint = endpoint.parse().map_err(|err| {
                        WsError::InitError(format!(
                            "WebSocket Protocol Error: invalid path={:?}: {:?}",
                            path, err
                        ))
                    })?;
                    let mut parser = ws::WebSocketServer::new_server();
                    let mut buffer = BufferVec::new(8096);
                    let written =
                        parser
                            .server_accept(key, None, buffer.write())
                            .map_err(|err| {
                                WsError::InitError(format!(
                                    "WebSocket Protocol Error: accept fail: {:?}",
                                    err
                                ))
                            })?;
                    buffer.advance(written);
                    conn.write_response(buffer.read()).unwrap();
                    let mut state = WsServerHandshakeState::None;
                    std::mem::swap(&mut state, &mut self.server_handshake_state);
                    match state {
                        WsServerHandshakeState::SendResponse(_, _, conn, _) => {
                            let (stream, buffer) = conn.into_raw();
                            self.read_buffer = buffer;
                            self.cfg.endpoint = endpoint;
                            self.connected = Some((stream, parser));
                            return Ok(());
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
    }

    pub fn from_http_conn(
        conn: HttpConnServer,
        path: &str,
        frame_size: usize,
        ctx: ws::WebSocketContext,
        alive: Alive,
    ) -> Result<Self, WsError> {
        let cfg = WsStreamConfig {
            endpoint: "/".parse().unwrap(), // change after the handshake
            frame_size,
            keep_alive: None,
            auto_reconnect: false,
            alive,
        };
        let mut out = Self::init(cfg);
        out.server_handshake_state = WsServerHandshakeState::SendResponse(
            Instant::now(),
            ctx.sec_websocket_key,
            conn,
            path.to_owned(),
        );
        match out.server_handshake(None, None) {
            Ok(_) => {}
            Err(WsError::WouldBlock) => {}
            Err(err) => return Err(err),
        }
        Ok(out)
    }

    pub fn accept(
        stream: TcpStream,
        tls_cfg: Option<TlsServerConfig>,
        alive: Alive,
        frame_size: usize,
        timeout: Duration,
    ) -> Result<Self, WsError> {
        let conn = HttpConnServer::accept(stream, tls_cfg, Some(2048)).unwrap();
        let cfg = WsStreamConfig {
            endpoint: "/".parse().unwrap(), // change after the handshake
            frame_size,
            keep_alive: None,
            auto_reconnect: false,
            alive,
        };
        let mut out = Self::init(cfg);
        match out.server_handshake(Some(conn), Some(timeout)) {
            Ok(_) => {}
            Err(WsError::WouldBlock) => {}
            Err(err) => return Err(err),
        }
        Ok(out)
    }

    pub fn write_ty(&mut self, ty: WsDataType, buf: &[u8]) -> Result<(), WsError> {
        self.server_handshake(None, None)?;
        self.write_msg(ty.into(), buf)
    }

    /// Write Binary message
    ///
    /// Errors:
    ///  * [`WsError::ServerDisconnected`]
    pub fn write(&mut self, buf: &[u8]) -> Result<(), WsError> {
        self.server_handshake(None, None)?;
        self.write_msg(ws::WebSocketSendMessageType::Binary, buf)
    }

    /// Read Binary/Text message
    /// It will process other type messages internal, e.g. ping/pong. In this case, it
    /// will continue the reading until a binary/text message comes.
    ///
    /// Errors:
    ///  * [`WsError::WouldBlock`]: remote side is not ready, retry later.
    ///  * [`WsError::InitError`]: error in processing reconnect staff
    ///  * [`WsError::ServerDisconnected`]: the connection is disconnected or it's shutdown, retry later.
    pub fn read(&mut self, data: &mut Vec<u8>) -> Result<WsDataType, WsError> {
        self.server_handshake(None, None)?;
        self.generic_read(data)
    }
}

impl<T: ws::WebSocketType, R: RngCore> WsStream<R, T> {
    fn init(mut cfg: WsStreamConfig) -> Self {
        if cfg.frame_size == 0 {
            cfg.frame_size = 4096;
        }

        let buffer = vec![0u8; cfg.frame_size * 2];
        let read_buffer = BufferVec::new(cfg.frame_size * 2);
        let wbuf = WriteBuffer::new(cfg.frame_size * 2);
        Self {
            cfg,
            connected: None,
            client_handshake_state: WsClientHandshakeState::None,
            server_handshake_state: WsServerHandshakeState::None,
            buffer,
            read_buffer,
            wbuf,
            connection_epoch: AtomicUsize::new(1),
            last_msg_time: Instant::now(),
            continuation_buffer: ExpandVec::new(),
        }
    }

    // fn flush_buffer(
    //     &mut self,
    //     size: usize,
    //     timeout: Option<(Instant, Duration)>,
    // ) -> Result<(), WsError> {
    //     loop {
    //         if let Some((start, timeout)) = timeout {
    //             if start.elapsed() > timeout {
    //                 return Err(WsError::InitError("timeout".into()));
    //             }
    //         }
    //         self.check_connected()?;
    //         let (stream, _) = self.connected.as_mut().unwrap();
    //         if let Err(err) = stream.write_all(&self.buffer[..size]) {
    //             if err.kind() != ErrorKind::WouldBlock {
    //                 return Err(WsError::InitError(format!(
    //                     "Handshake write error: {:?}",
    //                     err
    //                 )));
    //             }
    //             // maybe the buffer is full
    //             thread::sleep(Duration::from_millis(10));
    //             continue;
    //         }
    //         break;
    //     }
    //     Ok(())
    // }

    /// Shutdown the WebsocketStream, it will return ServerDisconnected to everywhere.
    pub fn shutdown(&self) {
        self.cfg.alive.shutdown()
    }

    /// Fill the read_buffer, returning zero means it fills nothing (WouldBlock/buffer full)
    ///
    /// Errors:
    ///  * [`WsError::ServerDisconnected`]: remote side close the connection
    fn fill_read_buffer(&mut self) -> Result<usize, WsError> {
        let buffer = self.read_buffer.write();
        if buffer.len() == 0 {
            return Ok(0); // buffer full
        }
        let size = match self.connected.as_mut().unwrap().0.read(buffer) {
            Ok(0) => {
                self.disconnect("remote side close the connection");
                return Err(WsError::ServerDisconnected { temporary: true });
            }
            Ok(n) => n,
            Err(err) if [ErrorKind::TimedOut, ErrorKind::WouldBlock].contains(&err.kind()) => {
                return Ok(0);
            }
            Err(err) => {
                glog::error!("read fail: {:?}", err);
                self.disconnect("read fail");
                return Err(WsError::ServerDisconnected { temporary: true });
            }
        };
        self.read_buffer.advance(size);
        Ok(size)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.is_some()
    }

    pub fn disconnect(&mut self, reason: &str) {
        glog::info!(
            "[{}] disconnect cause by {}",
            self.cfg.endpoint.tag(),
            reason
        );
        self.connected = None;
    }

    pub fn blocking<F, E>(&mut self, ms: u64, mut f: F) -> Result<E, WsError>
    where
        F: FnMut(&mut Self) -> Result<E, WsError>,
    {
        let sleep = Duration::from_millis(ms);
        loop {
            match f(self) {
                Ok(v) => return Ok(v),
                Err(WsError::WouldBlock) => {
                    std::thread::sleep(sleep);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn check_alive(&self) -> Result<(), WsError> {
        if !self.cfg.alive.is_alive() {
            return Err(WsError::ServerDisconnected { temporary: false });
        }
        return Ok(());
    }

    /// Errors:
    /// * [`WsError::ServerDisconnected`]
    fn check_connected(&mut self) -> Result<(), WsError> {
        let is_alive = self.check_alive();
        if is_alive.is_err() {
            self.connected = None;
        }
        is_alive?;
        if self.connected.is_none() {
            return Err(WsError::ServerDisconnected {
                temporary: self.cfg.auto_reconnect,
            });
        }
        Ok(())
    }

    /// Read Binary/Text message
    /// It will process other type messages internal, e.g. ping/pong. In this case, it
    /// will continue the reading until a binary/text message comes.
    ///
    /// Errors:
    ///  * [`WsError::WouldBlock`]: remote side is not ready, retry later.
    ///  * [`WsError::InitError`]: error in processing reconnect staff
    ///  * [`WsError::ServerDisconnected`]: the connection is disconnected or it's shutdown, retry later.
    fn generic_read(&mut self, data: &mut Vec<u8>) -> Result<WsDataType, WsError> {
        // glog::info!("origin data: {:?}", String::from_utf8_lossy(&data));
        const PING_MSG: &[u8] = b"1234567890abcdefghijklmnopqrstuvwxyz";
        type Type = ws::WebSocketReceiveMessageType;
        loop {
            self.check_connected()?;
            // let old_len = self.continuation_buffer.len();
            let mut ty = match self.read_msg() {
                Ok(ty) => ty,
                Err(WsError::WouldBlock) => {
                    // keepalive
                    if let Some(du) = self.cfg.keep_alive {
                        if self.continuation_buffer.len() == 0 && self.last_msg_time.elapsed() > du
                        {
                            self.write_msg(ws::WebSocketSendMessageType::Ping, PING_MSG)?;
                            // glog::info!("write ping");
                        }
                    }
                    self.flush_wbuf()?;
                    return Err(WsError::WouldBlock);
                }
                Err(err) => return Err(err.into()),
            };
            // glog::info!(
            //     "{} data type: {:?} {}",
            //     self.cfg.endpoint.to_string(),
            //     ty,
            //     crate::utils::summarize_str(
            //         &String::from_utf8_lossy(&self.continuation_buffer),
            //         2048
            //     )
            // );
            let last_msg = self.continuation_buffer.last_msg();
            if [Type::Binary, Type::Text].contains(&ty) && last_msg == Some(PING_MSG) {
                ty = Type::Pong;
            }
            let ty = match ty {
                Type::Binary => WsDataType::Binary,
                Type::Text => WsDataType::Text,
                Type::CloseCompleted => {
                    self.disconnect("remote request to close");
                    return Err(WsError::ServerDisconnected { temporary: true });
                }
                Type::CloseMustReply => {
                    self.continuation_buffer.pop();
                    self.write_msg(ws::WebSocketSendMessageType::CloseReply, &[])?;
                    continue;
                }
                Type::Ping => {
                    let msg = self.continuation_buffer.pop().unwrap_or(Vec::new());
                    let result = self.write_msg(ws::WebSocketSendMessageType::Pong, &msg);
                    result?;
                    continue;
                }
                Type::Pong => {
                    self.continuation_buffer.pop();
                    continue;
                }
            };
            // if data.len() == 0 {
            //     std::mem::swap(data, &mut self.continuation_buffer);
            // } else {
            self.continuation_buffer.move_to(data);
            // }
            // glog::info!("continuation: {}/{}, wbuf: {}", self.continuation_buffer.len(), self.continuation_buffer.capacity(), self.wbuf.buffered());
            // if data.len() + self.continuation_buffer.len() > 1024 * 1024 {
            //     glog::warn!(
            //         "received data too large: {}+{}, force disconnect",
            //         data.len(),
            //         self.continuation_buffer.len()
            //     );
            //     return Err(WsError::ServerDisconnected { temporary: false });
            // }

            // glog::info!("write out: {:?}", String::from_utf8_lossy(&data));
            return Ok(ty);
        }
    }

    fn is_read_frame_incomplete(result: &ws::Result<ws::WebSocketReadResult>) -> bool {
        match result {
            Ok(result) => {
                // we will get the empty result if our read_buffer empty
                result.len_from == 0
                    && result.len_to == 0
                    && result.end_of_message == false
                    && matches!(
                        result.message_type,
                        ws::WebSocketReceiveMessageType::Text
                            | ws::WebSocketReceiveMessageType::Binary
                    )
            }
            Err(ws::Error::ReadFrameIncomplete) => true,
            _ => false,
        }
    }

    // fn rotate_read_buffer(&mut self, buffer_offset: usize) {
    //     self.read_buffer.rotate_left(buffer_offset);
    // }

    fn flush_wbuf(&mut self) -> Result<(), WsError> {
        self.check_connected()?;
        let stream = &mut self.connected.as_mut().unwrap().0;
        self.wbuf.flush_buffer(stream).map_err(|err| match err {
            IOError::WouldBlock => WsError::WouldBlock,
            IOError::EOF { temporary } => WsError::ServerDisconnected { temporary },
            _ => WsError::ServerDisconnected { temporary: false },
        })?;
        Ok(())
    }

    /// Read a single message, it may come from local buffer or remote side.
    /// concatenate the continuation frames, extend to `data`,
    /// that's why we need `&mut Vec<u8>`
    ///
    /// Errors:
    ///  * [`WsError::WouldBlock`]: remote side is not ready, retry later.
    ///  * [`WsError::InitError`]: error in processing reconnect staff
    ///  * [`WsError::ServerDisconnected`]: the connection is disconnected, retry later.
    fn read_msg(&mut self) -> Result<ws::WebSocketReceiveMessageType, WsError> {
        let mut ty;
        // let mut written = 0;

        let mut loop_cnt = 0;
        const INSANITY_LOOP_CNT: usize = 10000;
        loop {
            loop_cnt += 1;
            let is_insanity = loop_cnt >= INSANITY_LOOP_CNT;
            if loop_cnt > INSANITY_LOOP_CNT + 1000 {
                self.disconnect("sanity check");
                glog::error!("[sanity check fail] disconnect");
                return Err(WsError::ServerDisconnected { temporary: false });
            }
            self.check_connected()?;
            let from = self.read_buffer.read();
            let parser = &mut self.connected.as_mut().unwrap().1;
            let result = parser.read(from, &mut self.buffer);
            if is_insanity {
                glog::error!("[sanity check fail] read result: {:?}", result);
            }
            if Self::is_read_frame_incomplete(&result) {
                if self.fill_read_buffer()? == 0 {
                    if self.continuation_buffer.len() > 0 {
                        // glog::info!("break read: {}", self.continuation_buffer.len());
                    }
                    return Err(WsError::WouldBlock);
                }
                continue;
            }
            let result = result.map_err(|err| {
                self.disconnect("parser read fail");
                glog::error!("parser.read fail: {:?}", err);
                WsError::ServerDisconnected { temporary: true }
            })?;
            ty = result.message_type;
            // glog::info!("recv data: {:?} -> {}, {:?}", ty, result.len_to, result);
            self.read_buffer.rotate_left(result.len_from);
            // written += result.len_to;
            if result.close_status.is_some() {
                if ty == ws::WebSocketReceiveMessageType::Text
                    && self.buffer.len() >= 2
                    && result.len_to >= 2
                {
                    // the frist 2 bytes is the error code for protocol error
                    glog::error!(
                        "protocol error: {}",
                        String::from_utf8_lossy(&self.buffer[2..result.len_to])
                    );
                }
                self.disconnect("protocol error");
                return Err(WsError::ServerDisconnected { temporary: true });
            }

            self.continuation_buffer.push(&self.buffer[..result.len_to]);
            if result.end_of_message {
                break;
            }
        }

        self.last_msg_time = Instant::now();
        Ok(ty)
    }

    pub fn epoch(&self) -> usize {
        self.connection_epoch.load(Ordering::SeqCst)
    }

    /// Write a message to remote, no need to worry about the size, it will split them into suitable size.
    /// It's blocking
    ///
    /// Errors:
    ///  * [`WsError::ServerDisconnected`]
    fn write_msg(&mut self, ty: ws::WebSocketSendMessageType, buf: &[u8]) -> Result<(), WsError> {
        // glog::info!("write data {:?} -> {}", ty, buf.len());
        let mut buffer_offset = 0;
        let mut written = 0;
        while written < buf.len() {
            loop {
                // split the message to the buffer, until the buffer is full.
                self.check_connected()?;
                let from = if (&buf[written..]).len() > self.cfg.frame_size {
                    &buf[written..written + self.cfg.frame_size]
                } else {
                    &buf[written..]
                };
                let to = &mut self.buffer[buffer_offset..];
                let end_of_message = written + from.len() == buf.len();
                let parser = &mut self.connected.as_mut().unwrap().1;
                let size = match parser.write(ty, end_of_message, from, to) {
                    Ok(size) => size,
                    Err(ws::Error::WriteToBufferTooSmall) => break,
                    Err(ws::Error::WebSocketNotOpen) => {
                        self.disconnect("wrong ws state");
                        glog::error!("error: WebSocketNotOpen, should not happen");
                        return Err(WsError::ServerDisconnected { temporary: true });
                    }
                    Err(_) => unreachable!(),
                };
                // glog::info!(
                //     "{} write {:?}, {}, {}",
                //     self.cfg.endpoint.to_string(),
                //     ty,
                //     crate::utils::summarize_str(&String::from_utf8_lossy(from), 2048),
                //     end_of_message,
                // );
                written += from.len();
                buffer_offset += size;
                if written == buf.len() {
                    break;
                }
            }

            self.check_connected()?;
            let stream = &mut self.connected.as_mut().unwrap().0;
            match self.wbuf.must_write(stream, &self.buffer[..buffer_offset]) {
                Ok(_) => {}
                Err(err) => {
                    self.disconnect("write fail");
                    glog::error!("write fail: {:?}", err);
                    return Err(WsError::ServerDisconnected { temporary: true });
                }
            }

            buffer_offset = 0;
        }

        self.last_msg_time = Instant::now();
        Ok(())
    }
}

pub fn read_handshake_response(
    response: &HttpResponse,
    sec_websocket_key: &ws::WebSocketKey,
) -> Result<Option<ws::WebSocketSubProtocol>, ws::Error> {
    use core::str;
    match response.status.into() {
        101u16 => {
            // we are ok
        }
        code => {
            return Err(ws::Error::HttpResponseCodeInvalid(Some(code)));
        }
    };

    let sec_websocket_protocol: Option<ws::WebSocketSubProtocol> = None;
    for (name, value) in response.headers.iter() {
        match name.as_str() {
            "Sec-WebSocket-Accept" => {
                let mut output = [0; 28];
                build_accept_string(sec_websocket_key, &mut output)?;

                let expected_accept_string = str::from_utf8(&output)?;
                let actual_accept_string = value.as_str();

                if actual_accept_string != expected_accept_string {
                    return Err(ws::Error::AcceptStringInvalid);
                }
            }
            _ => {
                // ignore all other headers
            }
        }
    }

    Ok(sec_websocket_protocol)
}

fn build_accept_string(sec_websocket_key: &ws::WebSocketKey, output: &mut [u8]) -> ws::Result<()> {
    use heapless::consts::*;
    use heapless::String;
    // concatenate the key with a known websocket GUID (as per the spec)
    let mut accept_string: String<U64> = String::new();
    accept_string.push_str(sec_websocket_key)?;
    accept_string.push_str("258EAFA5-E914-47DA-95CA-C5AB0DC85B11")?;

    // calculate the base64 encoded sha1 hash of the accept string above
    let sha1 = Sha1::from(&accept_string);
    let input = sha1.digest().bytes();
    let data = base64::encode(&input); // no need for slices since the output WILL be 28 bytes
    output.copy_from_slice(data.as_bytes());
    Ok(())
}

pub fn get_websocket_ctx(req: &mut HttpRequestReader) -> ws::Result<Option<ws::WebSocketContext>> {
    use core::str;
    use heapless::consts::*;
    use heapless::{String, Vec};

    req.headers(|headers| {
        let mut sec_websocket_protocol_list: Vec<String<U24>, U3> = Vec::new();
        let mut is_websocket_request = false;
        let mut sec_websocket_key = String::new();
        for item in headers.iter() {
            match item.name {
                "Upgrade" => is_websocket_request = str::from_utf8(item.value)? == "websocket",
                "Sec-WebSocket-Protocol" => {
                    // extract a csv list of supported sub protocols
                    for item in str::from_utf8(item.value)?.split(", ") {
                        if sec_websocket_protocol_list.len()
                            < sec_websocket_protocol_list.capacity()
                        {
                            // it is safe to unwrap here because we have checked
                            // the size of the list beforehand
                            sec_websocket_protocol_list
                                .push(String::from(item))
                                .unwrap();
                        }
                    }
                }
                "Sec-WebSocket-Key" => {
                    sec_websocket_key = String::from(str::from_utf8(item.value)?);
                }
                &_ => {
                    // ignore all other headers
                }
            }
        }
        Ok(if is_websocket_request {
            Some(ws::WebSocketContext {
                sec_websocket_protocol_list,
                sec_websocket_key,
            })
        } else {
            None
        })
    })
}
