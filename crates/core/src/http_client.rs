//! Minimal blocking HTTPS client (rustls, so no OpenSSL to ship) shared by the update checker, ESI price/ID lookups and paste uploads.

use std::io::Read;
use std::time::Duration;

use crate::log::Scope;

const SLOG: Scope = Scope::new("http_client");
const USER_AGENT: &str = "EVE-Maj-Preview";

#[derive(Debug, Default, Clone)]
pub struct FetchOptions<'a> {
    pub content_type: Option<&'a str>,
    /// When set, the request is a POST carrying this body.
    pub payload: Option<&'a [u8]>,
    pub extra_headers: &'a [(&'a str, &'a str)],
}

/// Reusable client; keeps connections alive between requests like the Zig build's std.http.Client.
#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent(USER_AGENT)
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(60))
            .build();
        Self { agent }
    }

    /// Issues a GET (or, with a payload, POST) request and returns the response body if it got a 200, else None.
    pub fn fetch(&self, url: &str, options: &FetchOptions<'_>) -> Option<Vec<u8>> {
        let method = if options.payload.is_some() { "POST" } else { "GET" };
        let mut request = self.agent.request(method, url);
        if let Some(ct) = options.content_type {
            request = request.set("Content-Type", ct);
        }
        for (name, value) in options.extra_headers {
            request = request.set(name, value);
        }

        let result = match options.payload {
            Some(body) => request.send_bytes(body),
            None => request.call(),
        };

        let response = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                SLOG.warn(format_args!("HTTP request to {url} returned status {code}: {body}"));
                return None;
            }
            Err(err) => {
                SLOG.warn(format_args!("HTTP request to {url} failed: {err}"));
                return None;
            }
        };

        let status = response.status();
        let mut body = Vec::new();
        if let Err(err) = response.into_reader().read_to_end(&mut body) {
            SLOG.warn(format_args!("Failed to read HTTP response body for {url}: {err}"));
            return None;
        }
        if status != 200 {
            SLOG.warn(format_args!("HTTP request to {url} returned status {status}: {}", String::from_utf8_lossy(&body)));
            return None;
        }
        Some(body)
    }
}
