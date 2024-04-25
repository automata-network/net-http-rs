// http_client is a hight-level http client (than http conn client)

use std::prelude::v1::*;

use base::time::Time;

use crate::{ConnPool, HttpConnBlockingClient, HttpConnError, HttpRequestBuilder, HttpResponse};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug)]
pub struct HttpClient {
    idle_time: Option<Duration>,
    conns: Mutex<BTreeMap<String, ConnPool<HttpConnBlockingClient>>>,
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(BTreeMap::new()),
            idle_time: Some(Duration::from_secs(10)),
        }
    }

    pub fn send(
        &self,
        req: &mut HttpRequestBuilder,
        dur: Option<Duration>,
    ) -> Result<HttpResponse, HttpConnError> {
        let conn_key = req.uri().stream_info().unwrap();
        let conn_pool = {
            let mut conns = self.conns.lock().unwrap();
            conns
                .entry(conn_key)
                .or_insert_with(|| ConnPool::new(self.idle_time))
                .clone()
        };
        let deadline = dur.map(|dur| Time::now() + dur);
        let response = loop {
            let mut conn =
                conn_pool.get_or(|| HttpConnBlockingClient::connect(req.uri().clone()))?;
            let timeout = deadline.map(|d| d.saturating_duration_since(Time::now()));
            if let Some(timeout) = &timeout {
                if timeout.is_zero() {
                    return Err(HttpConnError::Timeout);
                }
            }
            let result = conn.send(req, timeout);
            let response = match result {
                Ok(response) => response,
                Err(HttpConnError::WouldBlock) => {
                    // blocking should not have would block
                    return Err(HttpConnError::Timeout);
                }
                Err(HttpConnError::EOF) if conn.is_reused() => {
                    continue;
                }
                Err(err) => return Err(err),
            };
            conn.reuse();
            break response;
        };
        Ok(response)
    }
}
