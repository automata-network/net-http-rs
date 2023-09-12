use std::{net::SocketAddr, prelude::v1::*};

use http_req;

use bytes::BufferVec;
use gsrs::deref_with_lifetime;
use gsrs::*;
use http_req::request::RequestBuilder;
pub use http_req::response::StatusCode;
use http_req::response::{find_slice, Headers};
use net::StreamTrait;
use std::io::ErrorKind;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Options,
    Delete,
    Head,
    Connect,
    Unknown(String),
}

impl From<HttpMethod> for http_req::request::Method {
    fn from(m: HttpMethod) -> Self {
        use http_req::request::Method::*;
        match m {
            HttpMethod::Get => GET,
            HttpMethod::Post => POST,
            HttpMethod::Put => PUT,
            HttpMethod::Options => OPTIONS,
            HttpMethod::Delete => DELETE,
            HttpMethod::Head => HEAD,
            HttpMethod::Connect => unreachable!(),
            HttpMethod::Unknown(_) => unreachable!(),
        }
    }
}

impl From<&str> for HttpMethod {
    fn from(method: &str) -> Self {
        if method.eq_ignore_ascii_case("get") {
            return Self::Get;
        } else if method.eq_ignore_ascii_case("post") {
            return Self::Post;
        } else if method.eq_ignore_ascii_case("options") {
            return Self::Options;
        } else if method.eq_ignore_ascii_case("head") {
            return Self::Head;
        } else if method.eq_ignore_ascii_case("connect") {
            return Self::Connect;
        } else if method.eq_ignore_ascii_case("delete") {
            return Self::Delete;
        } else if method.eq_ignore_ascii_case("put") {
            return Self::Put;
        } else {
            return Self::Unknown(method.into());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HttpError {
    ReadEOF,
    InvalidURI(String),
    ParseError(httparse::Error),
    ParseResponseError(String),
    InvalidResponse,
    BodyTooLarge,
}
pub struct HttpRequestReaderEx<'a> {
    srs: SRS<Vec<u8>, HttpRequestReaderRef<'a>>,
    path: Option<String>,
    method: HttpMethod,
    request_size: usize,
    peer_ip: Option<SocketAddr>,
}

pub type HttpRequestReader = HttpRequestReaderEx<'static>;

unsafe impl Send for HttpRequestReader {}

#[derive(Default)]
struct HttpRequestReaderRef<'a> {
    headers: Vec<httparse::Header<'a>>,
    body: &'a [u8],
}
deref_with_lifetime!(HttpRequestReaderRef);

#[derive(Debug)]
pub enum HttpRequestReadState {
    Empty,
    ReadBody(BufferVec),
}

impl HttpRequestReadState {
    pub fn take_buffer(&mut self) -> Option<BufferVec> {
        match self {
            Self::Empty => None,
            Self::ReadBody(_) => {
                let mut state = Self::Empty;
                std::mem::swap(self, &mut state);
                match state {
                    Self::Empty => unreachable!(),
                    Self::ReadBody(buf) => Some(buf),
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum HttpResponseReadState {
    Empty,
    ReadBody(http_req::response::Response, BufferVec),
    ReadChunkedBody(http_req::response::Response, Vec<BufferVec>),
}

impl HttpResponseReadState {
    pub fn swap_to_empty(&mut self) -> Self {
        let mut state = Self::Empty;
        std::mem::swap(&mut state, self);
        state
    }
}

impl<'a> HttpRequestReaderEx<'a> {
    pub fn new(buf: Vec<u8>, peer_ip: Option<SocketAddr>) -> Result<Option<Self>, HttpError> {
        let request_size = buf.len();
        let mut reader = Self {
            srs: SRS::new(buf),
            method: HttpMethod::Get,
            path: None,
            request_size,
            peer_ip,
        };
        let (method, path, status) = reader.srs.with(|user, owner| {
            std::mem::swap(&mut user.headers, &mut vec![httparse::EMPTY_HEADER; 64]);
            let mut req = httparse::Request::new(&mut user.headers);
            match req.parse(&owner) {
                Ok(status) => {
                    let mut path = None;
                    let mut method = HttpMethod::Get;
                    if status.is_complete() {
                        path = match req.path {
                            Some(path) => Some(path.to_owned()),
                            None => None,
                        };
                        method = req.method.unwrap_or("").into();
                        user.body = &owner[status.unwrap()..];
                    }
                    Ok((method, path, status))
                }
                Err(err) => Err(HttpError::ParseError(err)),
            }
        })?;
        if status.is_partial() {
            return Ok(None);
        }
        reader.method = method;
        reader.path = path;
        Ok(Some(reader))
    }

    pub fn method(&self) -> HttpMethod {
        self.method.clone()
    }

    pub fn path(&self) -> &str {
        match self.path.as_ref() {
            Some(n) => n.as_str(),
            None => "/",
        }
    }

    pub fn request_size(&self) -> usize {
        self.request_size
    }

    pub fn origin(&mut self) -> Option<String> {
        self.headers(|headers| {
            for header in headers {
                if header.name == "Origin" {
                    return Some(String::from_utf8_lossy(header.value).to_string());
                }
            }
            return None;
        })
    }

    pub fn headers<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut [httparse::Header<'_>]) -> R + 'static,
        R: 'static,
    {
        self.srs.with(|user, _| f(&mut user.headers))
    }

    pub fn body(&self) -> &[u8] {
        self.srs.get_ref(|user, _| user.body)
    }

    pub fn peer_ip(&self) -> Option<&SocketAddr> {
        self.peer_ip.as_ref()
    }

    pub fn read_from<R>(
        stream: &mut R,
        header_buffer: &mut BufferVec,
        state: &mut HttpRequestReadState,
        max_body: Option<usize>,
    ) -> Result<Option<HttpRequestReaderEx<'a>>, HttpError>
    where
        R: StreamTrait,
    {
        loop {
            match state {
                HttpRequestReadState::Empty => {
                    loop {
                        match Self::get_request_size(header_buffer.read())? {
                            Some(length) => {
                                if length > max_body.unwrap_or(100 << 20) {
                                    // It's all in memory, so maybe 100MB is the hard limitation.
                                    glog::error!("body is too large: {:?}", max_body);
                                    return Err(HttpError::BodyTooLarge);
                                }
                                if let Some(buf) = header_buffer.read_n(length) {
                                    let req = match Self::new(
                                        buf.to_vec(),
                                        stream.peer_addr().ok(),
                                    )? {
                                        Some(req) => req,
                                        None => {
                                            glog::error!("should panic, can't read request from buffer: {:?}", String::from_utf8_lossy(buf));
                                            return Err(HttpError::ReadEOF);
                                        }
                                    };
                                    header_buffer.rotate_left(length);
                                    return Ok(Some(req));
                                }
                                let mut new_buf = BufferVec::new(length);
                                header_buffer.move_to(&mut new_buf);
                                *state = HttpRequestReadState::ReadBody(new_buf);
                                break;
                            }
                            None => {}
                        };
                        if header_buffer.write().len() == 0 {
                            glog::info!("body: {}", String::from_utf8_lossy(header_buffer.read()));
                            // buffer size too small;
                            unreachable!();
                        }
                        match stream.read(header_buffer.write()) {
                            Ok(0) => return Err(HttpError::ReadEOF),
                            Ok(incoming_bytes) => {
                                header_buffer.advance(incoming_bytes);
                                continue;
                            }
                            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                                return Ok(None);
                            }
                            Err(_) => return Err(HttpError::ReadEOF),
                        };
                    }
                }
                HttpRequestReadState::ReadBody(buf) => loop {
                    match stream.read(buf.write()) {
                        Ok(0) => return Err(HttpError::ReadEOF),
                        Ok(incoming_bytes) => {
                            buf.advance(incoming_bytes);
                            if buf.write().len() == 0 {
                                let buf = state.take_buffer().unwrap();
                                let req =
                                    Self::new(buf.to_vec(), stream.peer_addr().ok())?.unwrap();
                                return Ok(Some(req));
                            }
                            continue;
                        }
                        Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(None),
                        Err(_) => return Err(HttpError::ReadEOF),
                    };
                },
            }
        }
    }

    pub fn get_request_size(buf: &[u8]) -> Result<Option<usize>, HttpError> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        let res = match req.parse(&buf) {
            Ok(res) => res,
            Err(httparse::Error::Token) => return Ok(None),
            Err(err) => return Err(HttpError::ParseError(err)),
        };
        if res.is_partial() {
            return Ok(None);
        }
        let content_length = {
            let mut len = 0;
            for header in &headers {
                if header.name.to_lowercase() == "content-length" {
                    match String::from_utf8_lossy(header.value).parse() {
                        Ok(val) => {
                            len = val;
                            break;
                        }
                        Err(_) => return Err(HttpError::ParseError(httparse::Error::HeaderValue)),
                    }
                }
            }
            len
        };
        Ok(Some(res.unwrap() + content_length))
    }
}

struct HttpRequestBuilderOwner {
    uri: Uri,
    body: Option<Vec<u8>>,
}

pub struct HttpRequestBuilder<'a>(SRS<HttpRequestBuilderOwner, RequestBuilderRef<'a>>);
struct RequestBuilderRef<'a>(RequestBuilder<'a>);
deref_with_lifetime!(RequestBuilderRef);
impl<'a> HttpRequestBuilder<'a> {
    pub fn new<M: Into<HttpMethod>>(method: M, uri: Uri, body: Option<Vec<u8>>) -> Self {
        let method = method.into();
        Self::new_ex(uri, body, |req| {
            req.method(method);
        })
    }

    pub fn new_ex<F>(uri: Uri, body: Option<Vec<u8>>, f: F) -> Self
    where
        F: FnOnce(&mut RequestBuilder<'_>) + 'static,
    {
        let owner = HttpRequestBuilderOwner { uri, body };
        let srs = SRS::create_with(owner, |owner| {
            let mut req = RequestBuilder::new(&owner.uri.uri);
            if let Some(body) = &owner.body {
                req.body(&body);
                req.header("Content-Length", &body.len().to_string());
            }
            f(&mut req);

            RequestBuilderRef(req)
        });
        Self(srs)
    }

    pub fn uri(&self) -> &Uri {
        self.0.get_ref(|_, owner| &owner.uri)
    }

    pub fn parse_msg(&mut self) -> Vec<u8> {
        self.0.with(|req, _| req.0.parse_msg())
    }
}

pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn read_from<R>(
        stream: &mut R,
        header_buffer: &mut BufferVec,
        state: &mut HttpResponseReadState,
    ) -> Result<Option<HttpResponse>, HttpError>
    where
        R: std::io::Read,
    {
        const CR_LF_2: [u8; 4] = [13, 10, 13, 10];
        loop {
            match state {
                HttpResponseReadState::Empty => {
                    match header_buffer.fill_with(stream) {
                        Ok(_) => {}
                        Err(err) if err.kind() == ErrorKind::WouldBlock => {
                            if header_buffer.len() == 0 {
                                return Ok(None);
                            }
                        }
                        Err(_) => return Err(HttpError::ReadEOF),
                    }
                    let body_idx = match find_slice(header_buffer.read(), &CR_LF_2) {
                        Some(n) => n,
                        None => {
                            if header_buffer.is_full() {
                                glog::error!("read response error: buffer too small or not http");
                                return Err(HttpError::ReadEOF);
                            }
                            return Ok(None);
                        }
                    };
                    let (head, remaining) = (
                        &header_buffer.read()[..body_idx],
                        &header_buffer.read()[body_idx..],
                    );

                    let response = match http_req::response::Response::from_head(head) {
                        Ok(response) => response,
                        Err(err) => {
                            return Err(HttpError::ParseResponseError(format!("{:?}", err)))
                        }
                    };

                    let length = match response.content_len() {
                        Some(length) => length,
                        None => {
                            // maybe we can try chunked encoding
                            let mut chunks = Vec::new();
                            let buf = &mut &remaining[..];
                            match read_chunks(buf, &mut chunks) {
                                ChunksReadState::NotChunked => 0,
                                ChunksReadState::BufferNotEnough
                                    if buf.len() == remaining.len() =>
                                {
                                    0
                                }
                                chunk_state => {
                                    let off = head.len() + remaining.len() - buf.len();
                                    header_buffer.rotate_left(off);
                                    match chunk_state {
                                        ChunksReadState::BufferNotEnough => {
                                            *state = HttpResponseReadState::ReadChunkedBody(
                                                response, chunks,
                                            );
                                            continue;
                                        }
                                        ChunksReadState::Finished => {
                                            let body: BufferVec = chunks.into();
                                            return Ok(Some(HttpResponse {
                                                headers: response.headers().clone(),
                                                status: response.status_code(),
                                                body: body.to_vec(),
                                            }));
                                        }
                                        ChunksReadState::IllegalBody(err) => panic!("{:?}", err),
                                        ChunksReadState::NotChunked => unreachable!(),
                                    }
                                }
                            }
                        }
                    };

                    if length >= remaining.len() {
                        let body = BufferVec::from_slice(remaining, length);
                        let off = head.len() + remaining.len();
                        header_buffer.rotate_left(off);
                        *state = HttpResponseReadState::ReadBody(response, body);
                    } else {
                        let body = remaining[..length].into();
                        let off = head.len() + length;
                        header_buffer.rotate_left(off);
                        return Ok(Some(HttpResponse {
                            headers: response.headers().clone(),
                            status: response.status_code(),
                            body,
                        }));
                    }
                }
                HttpResponseReadState::ReadChunkedBody(_, chunks) => {
                    match header_buffer.fill_with(stream) {
                        Ok(_) => {}
                        Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => {
                            glog::error!("read response error: {:?}", e);
                            return Err(HttpError::ReadEOF);
                        }
                    }
                    let buf = &mut header_buffer.read();
                    match read_chunks(buf, chunks) {
                        ChunksReadState::BufferNotEnough => {
                            let off = header_buffer.len() - buf.len();
                            header_buffer.rotate_left(off);
                            return Ok(None);
                        }
                        ChunksReadState::Finished => {
                            let off = header_buffer.len() - buf.len();
                            header_buffer.rotate_left(off);
                            match state.swap_to_empty() {
                                HttpResponseReadState::ReadChunkedBody(response, chunks) => {
                                    let body: BufferVec = chunks.into();
                                    return Ok(Some(HttpResponse {
                                        headers: response.headers().clone(),
                                        status: response.status_code(),
                                        body: body.to_vec(),
                                    }));
                                }
                                _ => unreachable!(),
                            }
                        }
                        ChunksReadState::IllegalBody(err) => panic!("{}", err),
                        ChunksReadState::NotChunked => unreachable!(),
                    }
                }
                HttpResponseReadState::ReadBody(_, buf) => match buf.fill_all_with(stream) {
                    Ok(_) => match state.swap_to_empty() {
                        HttpResponseReadState::ReadBody(response, buf) => {
                            return Ok(Some(HttpResponse {
                                headers: response.headers().clone(),
                                status: response.status_code(),
                                body: buf.to_vec(),
                            }))
                        }
                        _ => unreachable!(),
                    },
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        return Ok(None);
                    }
                    Err(e) => {
                        glog::error!("read response error: {:?}", e);
                        return Err(HttpError::ReadEOF);
                    }
                },
            }
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::with_capacity(1024 + self.body.len());
        let code: u16 = self.status.into();
        write!(
            buf,
            "HTTP/1.1 {} {}\r\n",
            code,
            self.status.reason().unwrap()
        )
        .unwrap();
        for (key, value) in self.headers.iter() {
            write!(buf, "{}: {}\r\n", key, value).unwrap();
        }

        write!(buf, "\r\n").unwrap();
        buf.extend_from_slice(&self.body);
        buf
    }
}

#[must_use]
pub struct HttpResponseBuilder {
    response: HttpResponse,
}

impl From<HttpResponseBuilder> for HttpResponse {
    fn from(b: HttpResponseBuilder) -> Self {
        b.response
    }
}

impl HttpResponseBuilder {
    pub fn new(code: u16) -> Self {
        let response = Self {
            response: HttpResponse {
                status: StatusCode::new(code),
                headers: Headers::new(),
                body: Vec::new(),
            },
        };
        response.body(Vec::new())
    }

    pub fn to_vec(&mut self) -> Vec<u8> {
        self.response.to_vec()
    }

    pub fn not_found() -> Self {
        Self::new(404)
    }

    pub fn redirect(url: &str) -> Self {
        Self::new(302).header("Location", url)
    }

    pub fn header(mut self, key: &str, val: &str) -> Self {
        self.response.headers.insert(key, val);
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self = self.header("Content-Length", &format!("{}", body.len()));
        self.response.body = body;
        self
    }

    pub fn content_type(self, ty: &str) -> Self {
        self.header("Content-Type", ty)
    }

    pub fn json(self, body: Vec<u8>) -> Self {
        self.content_type("application/json").body(body)
    }

    pub fn keep_alive(self) -> Self {
        self.header("Connection", "Keep-Alive")
    }

    pub fn close(self) -> Self {
        self.header("Connection", "Close")
    }

    pub fn build(self) -> HttpResponse {
        self.response
    }
}

#[derive(Debug)]
pub enum ChunksReadState {
    NotChunked,
    BufferNotEnough,
    Finished,
    IllegalBody(String),
}

fn read_chunk_header(buf: &mut &[u8]) -> Result<Option<usize>, ChunksReadState> {
    const CR_LF: [u8; 2] = [13, 10];
    let chunk_size_idx = buf.windows(CR_LF.len()).position(|win| win == &CR_LF);
    match chunk_size_idx {
        Some(chunk_size_idx) => {
            let length = match parse_hex_uint(&buf[..chunk_size_idx]) {
                Ok(length) => length,
                Err(err) => {
                    return Err(ChunksReadState::IllegalBody(format!(
                        "parse chunk header={} fail: {}",
                        String::from_utf8_lossy(&buf[..chunk_size_idx]),
                        err
                    )))
                }
            };
            *buf = &buf[chunk_size_idx + CR_LF.len()..];
            return Ok(Some(length));
        }
        None => return Ok(None),
    }
}

pub fn read_chunks(buf: &mut &[u8], chunks: &mut Vec<BufferVec>) -> ChunksReadState {
    const CR_LF: [u8; 2] = [13, 10];
    while !buf.is_empty() {
        match chunks.last_mut() {
            Some(chunk) if chunk.cap() == 0 => return ChunksReadState::Finished,
            Some(chunk) if chunk.is_full() => match read_chunk_header(buf) {
                Ok(Some(length)) => chunks.push(BufferVec::new(length + 2)),
                Ok(None) => {
                    if buf.len() < 16 {
                        return ChunksReadState::BufferNotEnough;
                    }
                    return ChunksReadState::IllegalBody("chunk header not found".into());
                }
                Err(state) => return state,
            },
            Some(chunk) => {
                let n = chunk.copy_from(&buf);
                *buf = &buf[n..];
                if chunk.is_full() {
                    if !chunk.ends_with(&CR_LF) {
                        return ChunksReadState::IllegalBody(
                            "chunks not follows with CR_LF".into(),
                        );
                    }
                    chunk.resize_cap(chunk.cap() - 2);
                }
            }
            None => match read_chunk_header(buf) {
                Ok(Some(length)) => chunks.push(BufferVec::new(length + 2)),
                Ok(None) => {
                    if buf.len() < 16 {
                        return ChunksReadState::BufferNotEnough;
                    }
                    return ChunksReadState::NotChunked;
                }
                Err(state) => return state,
            },
        }
    }
    match chunks.last() {
        Some(chunk) if chunk.cap() == 0 => ChunksReadState::Finished,
        None | Some(_) => ChunksReadState::BufferNotEnough,
    }
}

pub fn parse_hex_uint(data: &[u8]) -> Result<usize, &str> {
    let mut n = 0usize;
    for (i, v) in data.iter().enumerate() {
        if i == 16 {
            return Err("http chunk length too large");
        }

        let vv = match *v {
            b'0'..=b'9' => v - b'0',
            b'a'..=b'f' => v - b'a' + 10,
            b'A'..=b'F' => v - b'A' + 10,
            _ => return Err("invalid byte in chunk length"),
        };

        n <<= 4;
        n |= vv as usize;
    }

    Ok(n)
}

#[derive(Clone, Debug)]
pub struct Uri {
    uri: http_req::uri::Uri,
}

impl std::str::FromStr for Uri {
    type Err = http_req::error::Error;
    fn from_str(val: &str) -> Result<Self, Self::Err> {
        Ok(Self { uri: val.parse()? })
    }
}

impl ToString for Uri {
    fn to_string(&self) -> String {
        self.uri.to_string()
    }
}

impl Uri {
    pub fn new(url: &str) -> Result<Self, http_req::error::Error> {
        Ok(Self { uri: url.parse()? })
    }

    pub fn scheme(&self) -> &str {
        self.uri.scheme()
    }

    pub fn host(&self) -> Result<&str, HttpError> {
        self.uri
            .host()
            .ok_or_else(|| HttpError::InvalidURI("missing host".into()))
    }

    pub fn tag(&self) -> String {
        format!(
            "{}://{}:{}",
            self.scheme(),
            self.host().unwrap(),
            self.port().unwrap(),
        )
    }

    pub fn is_tls(&self) -> Result<bool, HttpError> {
        let tls = match self.uri.scheme() {
            "http" | "ws" => false,
            "https" | "wss" => true,
            schema => {
                return Err(HttpError::InvalidURI(format!(
                    "unexpected uri scheme: {}",
                    schema,
                )));
            }
        };
        Ok(tls)
    }

    pub fn port(&self) -> Result<u16, HttpError> {
        Ok(match self.uri.port() {
            Some(value) => value,
            None => match self.is_tls()? {
                true => 443,
                false => 80,
            },
        })
    }

    pub fn path(&self) -> &str {
        match self.uri.path() {
            None => "/",
            Some(value) => value,
        }
    }

    pub fn host_and_port(&self) -> Result<(&str, u16), HttpError> {
        Ok((self.host()?, self.port()?))
    }

    pub fn stream_info(&self) -> Result<String, HttpError> {
        Ok(format!(
            "{}://{}:{}",
            self.scheme(),
            self.host()?,
            self.port()?
        ))
    }
}
