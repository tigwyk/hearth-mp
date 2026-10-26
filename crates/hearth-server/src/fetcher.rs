//! Outbound HTTP for runtimes: the shared [`Fetcher`] seam and its test double.
//!
//! This lives in `hearth-server` rather than the gateway because both sides make
//! outbound calls: the gateway's auth providers verify credentials upstream, and
//! the room adapter's handoff allocator asks a fleet for a dedicated server.
//! Neither should depend on the other, so the seam is shared. The Worker
//! implementation (`hearth_gateway::WorkerFetcher`) stays in the gateway: it is
//! the only part that needs the Workers API.

use std::cell::RefCell;

/// One outbound HTTP call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpReq {
    /// `"GET"`, `"POST"`, ...
    pub method: &'static str,
    /// The absolute URL.
    pub url: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// The request body, if any.
    pub body: Option<String>,
}

/// An HTTP answer, decoded as text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpResp {
    /// The HTTP status.
    pub status: u16,
    /// The response body as text.
    pub body: String,
}

/// Performs outbound HTTP calls. Native tests use [`CannedFetcher`]; a Worker
/// uses the gateway's `WorkerFetcher`.
#[allow(async_fn_in_trait)]
pub trait Fetcher {
    /// Send one request. `Err` is a transport failure; any HTTP status is `Ok`.
    async fn send(&self, req: HttpReq) -> Result<HttpResp, String>;
}

/// A test fetcher: canned answers by URL prefix, plus the requests it saw.
#[derive(Default)]
pub struct CannedFetcher {
    routes: Vec<(String, HttpResp)>,
    requests: RefCell<Vec<HttpReq>>,
    failure: Option<String>,
}

impl CannedFetcher {
    /// A fetcher with no canned responses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer every request whose URL starts with `prefix` with `status`/`body`.
    pub fn route(mut self, prefix: &str, status: u16, body: &str) -> Self {
        self.routes.push((
            prefix.to_owned(),
            HttpResp {
                status,
                body: body.to_owned(),
            },
        ));
        self
    }

    /// Make the next call fail without an HTTP answer, like a timeout.
    pub fn failing(mut self, message: &str) -> Self {
        self.failure = Some(message.to_owned());
        self
    }

    /// Every request this fetcher was asked to send, in order.
    pub fn requests(&self) -> Vec<HttpReq> {
        self.requests.borrow().clone()
    }
}

impl Fetcher for CannedFetcher {
    async fn send(&self, req: HttpReq) -> Result<HttpResp, String> {
        if let Some(message) = &self.failure {
            self.requests.borrow_mut().push(req);
            return Err(message.clone());
        }
        let answer = self
            .routes
            .iter()
            .find(|(prefix, _)| req.url.starts_with(prefix))
            .map(|(_, response)| response.clone());
        self.requests.borrow_mut().push(req);
        answer.ok_or_else(|| "no canned response".to_owned())
    }
}
