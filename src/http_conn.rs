use std::prelude::v1::*;

use rustls;

use super::http_types::*;
use bytes::{BufferVec, IOError, WriteBuffer};
use net::*;
use rustls::ServerConfig;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub type TlsServerConfig = Arc<ServerConfig>;

#[derive(Debug)]
pub enum HttpConnError {
    EOF,
    WouldBlock,
    IoError(std::io::Error),
    Error(HttpError),
    Timeout,
}

impl From<IOError> for HttpConnError {
    fn from(err: IOError) -> Self {
        match err {
            IOError::EOF { .. } => Self::EOF,
            IOError::WouldBlock => Self::WouldBlock,
            IOError::Other(err) => Self::IoError(err),
        }
    }
}

impl From<std::io::Error> for HttpConnError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err)
    }
}

impl From<HttpError> for HttpConnError {
    fn from(err: HttpError) -> Self {
        if matches!(err, HttpError::ReadEOF) {
            Self::EOF
        } else {
            HttpConnError::Error(err)
        }
    }
}

#[derive(Debug)]
pub struct HttpConnServerType {}
#[derive(Debug)]
pub struct HttpConnClientType {}
#[derive(Debug)]
pub struct HttpConnBlockingClientType {}

pub type HttpConnClient = HttpConn<HttpConnClientType>;
pub type HttpConnBlockingClient = HttpConn<HttpConnBlockingClientType>;
pub type HttpConnServer = HttpConn<HttpConnServerType>;

#[derive(Debug)]
pub struct HttpConn<T> {
    _uri: Uri,
    stream: AnyStream,
    header_buffer: BufferVec,
    wbuf: WriteBuffer,
    request: HttpRequestReadState,
    response: HttpResponseReadState,
    inflight_id: Vec<usize>,
    max_body_length: Option<usize>,
    marker: std::marker::PhantomData<T>,
    // last_msg: Instant,
    closed: bool,
    request_cnt: usize,
}

impl<T> HttpConn<T> {
    pub fn into_raw(self) -> (AnyStream, BufferVec) {
        (self.stream, self.header_buffer)
    }

    fn new(uri: Uri, stream: AnyStream, max_body_length: Option<usize>) -> Self {
        Self {
            _uri: uri,
            stream,
            header_buffer: BufferVec::new(8096),
            wbuf: WriteBuffer::new(8 << 10),
            request: HttpRequestReadState::Empty,
            response: HttpResponseReadState::Empty,
            inflight_id: Vec::new(),
            max_body_length,
            marker: std::marker::PhantomData,
            // last_msg: Instant::now(),
            request_cnt: 0,
            closed: false,
        }
    }

    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    pub fn try_close(&mut self) {
        self.closed = true;
    }
}

impl HttpConnBlockingClient {
    pub fn connect(uri: Uri) -> Result<Self, HttpConnError> {
        let stream = AnyStreamBuilder {
            stream: TcpStream::connect(uri.host_and_port()?)?,
            proxy: false,
            tls_client: match uri.is_tls()? {
                true => Some(uri.host()?.into()),
                false => None,
            },
            tls_server: None,
        }
        .build()?;
        Ok(Self::new(uri, stream, None))
    }

    pub fn send(
        &mut self,
        req: &mut HttpRequestBuilder,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, HttpConnError> {
        let msg = req.parse_msg();
        self.stream.write_all(&msg)?;
        for _ in 0..10000 {
            self.stream.set_read_timeout(timeout)?;
            let response = HttpResponse::read_from(
                &mut self.stream,
                &mut self.header_buffer,
                &mut self.response,
            )?;
            if let Some(response) = response {
                return Ok(response);
            }
            // 1. WouldBlock
            // 2. buffer not enough
        }
        glog::error!("[sanity check] deadloop when reading response");
        return Err(HttpConnError::EOF);
    }
}

impl HttpConnClient {
    pub fn connect(uri: Uri) -> Result<Self, HttpConnError> {
        let stream = AnyStreamBuilder {
            stream: net::tcp::connect(uri.host_and_port()?)?,
            proxy: false,
            tls_client: match uri.is_tls()? {
                true => Some(uri.host()?.into()),
                false => None,
            },
            tls_server: None,
        }
        .build()?;
        Ok(Self::new(uri, stream, None))
    }

    pub fn inflight_len(&self) -> usize {
        self.inflight_id.len()
    }

    pub fn request_cnt(&self) -> usize {
        self.request_cnt
    }

    pub fn read_response(&mut self) -> Result<(usize, HttpResponse), HttpConnError> {
        self.wbuf.flush_buffer(&mut self.stream)?;
        if self.inflight_id.len() == 0 {
            return Err(HttpConnError::WouldBlock);
        }
        // self.keep_alive()?;
        loop {
            let response = HttpResponse::read_from(
                &mut self.stream,
                &mut self.header_buffer,
                &mut self.response,
            )?;
            match response {
                Some(response) => {
                    // glog::info!("inflight queue: {}-1", self.inflight_id.len());
                    let conn_id = self.inflight_id.remove(0);
                    if conn_id == usize::MAX {
                        // ignore ping request
                        continue;
                    }
                    self.request_cnt += 1;
                    return Ok((conn_id, response));
                }
                None => return Err(HttpConnError::WouldBlock),
            }
        }
    }

    pub fn write_raw_request(&mut self, req_id: usize, buf: &[u8]) -> Result<(), HttpConnError> {
        self.wbuf.must_write(&mut self.stream, buf)?;
        // glog::info!("inflight queue: {}+1", self.inflight_id.len());
        self.inflight_id.push(req_id);
        Ok(())
    }

    pub fn write_request(
        &mut self,
        req_id: usize,
        req: &mut HttpRequestBuilder,
    ) -> Result<(), HttpConnError> {
        // check whether the connection is alive
        if self.wbuf.idle_duration() > Duration::from_secs(10) {
            match self.stream.read(&mut []).map_err(IOError::from) {
                Ok(0) => return Err(IOError::EOF { temporary: false }.into()),
                Ok(_) => {}
                Err(IOError::WouldBlock) => {}
                Err(other) => return Err(other.into()),
            }
        }

        let msg = req.parse_msg();
        self.wbuf.must_write(&mut self.stream, &msg)?;
        // glog::info!("inflight queue: {}+1", self.inflight_id.len());
        self.inflight_id.push(req_id);
        Ok(())
    }
}

impl HttpConnServer {
    pub fn accept(
        stream: TcpStream,
        tls_cfg: Option<Arc<ServerConfig>>,
        max_body_length: Option<usize>,
    ) -> std::io::Result<HttpConnServer> {
        let stream = AnyStreamBuilder {
            stream,
            proxy: true,
            tls_client: None,
            tls_server: tls_cfg,
        }
        .build()?;
        stream.set_nonblocking(true)?;
        Ok(Self::new(
            "server".parse().unwrap(),
            stream,
            max_body_length,
        ))
    }

    pub fn read_request(&mut self) -> Result<Option<HttpRequestReader>, HttpConnError> {
        self.wbuf.flush_buffer(&mut self.stream)?;
        if self.closed {
            if self.inflight_id.len() > 0 {
                return Ok(None);
            }
            return Err(HttpConnError::EOF);
        }

        let req = HttpRequestReader::read_from(
            &mut self.stream,
            &mut self.header_buffer,
            &mut self.request,
            self.max_body_length,
        )?;
        if req.is_some() {
            self.inflight_id.push(0);
        }
        Ok(req)
    }

    pub fn write_response(&mut self, buf: &[u8]) -> Result<(), HttpConnError> {
        if self.inflight_id.len() == 0 {
            return Err(HttpConnError::EOF); // should not happen
        }
        self.wbuf.must_write(&mut self.stream, buf)?;
        self.inflight_id.pop();
        self.request_cnt += 1;
        Ok(())
    }
}

impl<T> std::cmp::PartialEq<HttpConn<T>> for HttpConn<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inflight_id.len() == other.inflight_id.len()
    }
}

impl<T> std::cmp::Eq for HttpConn<T> {}

impl<T> std::cmp::PartialOrd<HttpConn<T>> for HttpConn<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering;
        match self.inflight_id.len().partial_cmp(&other.inflight_id.len()) {
            Some(Ordering::Equal) => Some(Ordering::Equal),
            Some(Ordering::Greater) => Some(Ordering::Less),
            Some(Ordering::Less) => Some(Ordering::Greater),
            None => None,
        }
    }
}

impl<T> std::cmp::Ord for HttpConn<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self.inflight_id.len().cmp(&other.inflight_id.len()) {
            Ordering::Equal => Ordering::Equal,
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
        }
    }
}

pub struct HttpConnClientPool {
    pub name: String,
    pub uri: Uri,
    clients: Vec<HttpConnClient>,
}

impl HttpConnClientPool {
    pub fn new(size: usize, uri: &Uri) -> Result<Self, HttpConnError> {
        let mut clients = Vec::new();
        for _ in 0..size {
            clients.push(HttpConnClient::connect(uri.clone())?);
        }
        let name = uri.to_string();
        Ok(Self {
            name,
            uri: uri.clone(),
            clients,
        })
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn write_request(
        &mut self,
        req_id: usize,
        req: &mut HttpRequestBuilder,
    ) -> Result<(), HttpConnError> {
        let mut conn = self.clients.pop().unwrap();
        loop {
            let result = conn.write_request(req_id, req);
            if let Err(HttpConnError::EOF) = result {
                glog::error!(
                    "unexpected eof, try reconnect: dropped: {}",
                    conn.inflight_id.len()
                );
                conn = match HttpConnClient::connect(self.uri.clone()) {
                    Ok(conn) => conn,
                    Err(err) => {
                        glog::error!("reconnect fail: {:?}", err);
                        self.clients.push(conn);
                        return Err(err);
                    }
                };
                continue;
            }
            self.clients.push(conn);
            self.clients.sort_unstable();
            return result;
        }
    }

    pub fn read_response(&mut self) -> Result<(usize, HttpResponse), HttpConnError> {
        for client in &mut self.clients {
            match client.read_response() {
                Ok(response) => {
                    return Ok(response);
                }
                Err(HttpConnError::WouldBlock) => {}
                Err(err) => {
                    // try remove this
                    glog::error!("uri: {}, read error: {:?}", self.uri.to_string(), err);
                    let mut conn = match HttpConnClient::connect(self.uri.clone()) {
                        Ok(conn) => conn,
                        Err(err) => {
                            glog::error!("reconnect fail: {:?}", err);
                            return Err(err);
                        }
                    };
                    glog::info!("remove EOF client: inflight_len: {}", client.inflight_len());
                    std::mem::swap(client, &mut conn);
                    return Err(err);
                }
            }
        }
        Err(HttpConnError::WouldBlock)
    }

    pub fn inflight_len(&self) -> usize {
        let mut length = 0;
        for client in &self.clients {
            length += client.inflight_len();
        }
        length
    }
}
