//! Token-bucket rate-limiting reverse proxy for the public JSON-RPC endpoint.
//!
//! Sui-side: Reth serves the public RPC on port 8545 and the JWT-authenticated
//! Engine API on 8551. This proxy fronts the public port so untrusted traffic
//! never reaches Reth directly: requests are forwarded to the upstream over
//! loopback, per-client-IP request rates are bounded by a token bucket, and a
//! saturated proxy sheds load with `429` before memory grows. The sequencer's
//! own Engine API drive uses a separate authenticated port and is unaffected.
//!
//! Client identity: the direct peer's IP unless the peer is a configured
//! trusted proxy, in which case `X-Forwarded-For` is walked right-to-left and
//! the first non-trusted address wins. A client can therefore not choose its
//! bucket by forging the header.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Monotonic millisecond clock for bucket arithmetic. Wall-clock steps
/// (NTP corrections) must not mint or destroy tokens, so buckets read this
/// instead of `SystemTime`.
pub fn mono_now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// Refillable token bucket: `capacity` tokens, `refill_per_ms` tokens per
/// millisecond, fractional refill from explicit monotonic timestamps.
///
/// A new bucket starts full, so the first `capacity` requests of a fresh
/// client are admitted instantly (burst), after which the sustained
/// `refill_per_ms` rate applies.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_ms: f64,
    last_ms: u64,
}

impl TokenBucket {
    pub fn new(capacity: f64, refill_per_ms: f64) -> Self {
        Self {
            capacity,
            tokens: capacity,
            refill_per_ms,
            last_ms: mono_now_ms(),
        }
    }

    /// Takes one token. `Ok(())` when admitted; `Err(ms)` carries the
    /// monotonic-millisecond wait until the next token when denied.
    pub fn try_take(&mut self, now_ms: u64) -> Result<(), u64> {
        let elapsed = now_ms.saturating_sub(self.last_ms);
        self.last_ms = now_ms;
        self.tokens = (self.tokens + elapsed as f64 * self.refill_per_ms).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        // Time to refill one full token at the sustained rate; round up so
        // the suggested wait is never short.
        let wait = ((1.0 - self.tokens) / self.refill_per_ms).ceil() as u64;
        Err(wait.max(1))
    }
}

/// Rate limits admitted work: the deadline check against the
/// rate-limiting reverse proxy contract is performed in
/// [`crate::rpc_proxy::handler`] — this fn covers admission and carries the
/// bucket store.
#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<HashMap<IpAddr, (TokenBucket, u64)>>,
    capacity: f64,
    refill_per_ms: f64,
    max_keys: usize,
    idle_ms: u64,
}

/// Upper bound on tracked client buckets. Beyond it, brand-new client IPs
/// are limited outright — under a distributed flood the flood IPs are
/// exactly the new ones, while established clients keep their buckets.
pub const MAX_KEYS: usize = 65_536;

/// A bucket untouched for this long is dropped by [`RateLimiter::sweep`].
pub const IDLE_MS: u64 = 10 * 60 * 1000;

impl RateLimiter {
    pub fn new(capacity: f64, requests_per_second: f64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity,
            refill_per_ms: requests_per_second / 1000.0,
            max_keys: MAX_KEYS,
            idle_ms: IDLE_MS,
        }
    }

    /// Admits one request for `client`. `Err(ms)` is the suggested
    /// `Retry-After`, rounded up to whole milliseconds (min 1).
    pub fn check(&self, client: IpAddr, now_ms: u64) -> Result<(), u64> {
        let mut inner = self.inner.lock().expect("rate limiter lock");
        let entry = match inner.get_mut(&client) {
            Some((bucket, last_seen)) => {
                *last_seen = now_ms;
                bucket
            }
            None => {
                // The map is at cap and this is a brand-new client: limit it
                // outright instead of evicting an established bucket.
                if inner.len() >= self.max_keys {
                    return Err(1000);
                }
                let bucket = TokenBucket::new(self.capacity, self.refill_per_ms);
                inner.insert(client, (bucket, now_ms));
                &mut inner.get_mut(&client).expect("just inserted").0
            }
        };
        entry.try_take(now_ms)
    }

    /// Drops buckets idle past `idle_ms` at `now_ms`. Returns the count
    /// evicted. Called periodically from the proxy's sweep task.
    pub fn sweep(&self, now_ms: u64) -> usize {
        let mut inner = self.inner.lock().expect("rate limiter lock");
        let idle_ms = self.idle_ms;
        let before = inner.len();
        inner.retain(|_, (_, last_seen)| now_ms.saturating_sub(*last_seen) <= idle_ms);
        before - inner.len()
    }

    /// Test seam: tracked key count.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("rate limiter lock").len()
    }
}

/// Resolves the bucket key for a request from `peer`, the raw
/// `X-Forwarded-For` header, and the configured trusted-proxy set.
///
/// Direct peers not on the trusted list are taken as-is: everything they
/// send is ignored, forged headers included. Trusted proxies vouch for the
/// chain they forward: entries are walked right-to-left, trusted hops are
/// skipped, and the first non-trusted address is the client. A chain that
/// is entirely trusted proxies has no observable client, so the direct
/// peer wins. Malformed entries are skipped.
pub fn client_ip(peer: SocketAddr, xff: Option<&str>, trusted: &[IpAddr]) -> IpAddr {
    if !trusted.contains(&peer.ip()) {
        return peer.ip();
    }
    let Some(xff) = xff else {
        return peer.ip();
    };
    for entry in xff.split(',').rev() {
        let Ok(ip) = entry.trim().parse::<IpAddr>() else {
            continue;
        };
        if trusted.contains(&ip) {
            continue;
        }
        return ip;
    }
    // Every XFF entry was a trusted proxy: the observable client is the
    // direct peer.
    peer.ip()
}

/// Parsed and validated proxy rate settings (pure, testable seam between
/// the TOML config and the limiter).
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimits {
    pub capacity: f64,
    pub refill_per_ms: f64,
}

impl RateLimits {
    /// Builds from the config values. Fails closed: a zero/negative rate or
    /// a sub-one burst is a configuration error, not a silent disable.
    pub fn from_config(requests_per_second: f64, burst: u32) -> Result<Self, String> {
        if !requests_per_second.is_finite() || requests_per_second <= 0.0 {
            return Err(format!(
                "rpc_proxy.requests_per_second must be > 0, got {requests_per_second}"
            ));
        }
        if burst < 1 {
            return Err("rpc_proxy.burst must be >= 1".to_string());
        }
        Ok(Self {
            capacity: burst as f64,
            refill_per_ms: requests_per_second / 1000.0,
        })
    }
}

/// Upstream hop timeout for forwarded JSON-RPC calls. A hung Reth must not
/// pin proxy tasks and in-flight permits forever.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared per-proxy state handed to every request handler.
pub struct ProxyState {
    limiter: RateLimiter,
    http: reqwest::Client,
    upstream: String,
    max_body_bytes: usize,
    trusted_proxies: Vec<IpAddr>,
    in_flight: Arc<tokio::sync::Semaphore>,
    metrics: Arc<crate::metrics::PodseqMetrics>,
}

impl std::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyState")
            .field("upstream", &self.upstream)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish_non_exhaustive()
    }
}

impl ProxyState {
    /// Builds from validated [`RateLimits`] and the remaining config knobs.
    /// Configuration errors surface here, before anything binds.
    pub fn new(
        limits: RateLimits,
        upstream: String,
        max_body_bytes: usize,
        max_in_flight: usize,
        trusted_proxies: Vec<IpAddr>,
        metrics: Arc<crate::metrics::PodseqMetrics>,
    ) -> Result<Self, String> {
        if max_body_bytes == 0 {
            return Err("rpc_proxy.max_body_bytes must be > 0".to_string());
        }
        if max_in_flight == 0 {
            return Err("rpc_proxy.max_in_flight must be > 0".to_string());
        }
        if reqwest::Url::parse(&upstream).is_err() {
            return Err(format!("rpc_proxy.upstream is not a valid URL: {upstream}"));
        }
        let http = reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .map_err(|e| format!("rpc_proxy upstream client build failed: {e}"))?;
        Ok(Self {
            limiter: RateLimiter::new(limits.capacity, limits.refill_per_ms * 1000.0),
            http,
            upstream,
            max_body_bytes,
            trusted_proxies,
            in_flight: Arc::new(tokio::sync::Semaphore::new(max_in_flight)),
            metrics,
        })
    }
}

/// Builds the proxy router: POST on any path forwards; other methods get
/// axum's standard 405.
pub fn build_router(state: Arc<ProxyState>) -> axum::Router {
    use axum::routing::post;
    axum::Router::new()
        .route("/", post(handler))
        .route("/{*rest}", post(handler))
        .with_state(state)
}

/// Serves the rate-limiting proxy on `addr` until `shutdown` fires.
///
/// A background task sweeps idle client buckets every 60 s so a flood of
/// one-shot IPs cannot grow the bucket map without bound.
pub async fn serve(state: Arc<ProxyState>, addr: SocketAddr, shutdown: Arc<tokio::sync::Notify>) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %addr, error = %e, "failed to bind rpc proxy");
            return;
        }
    };
    tracing::info!(addr = %addr, upstream = %state.upstream, "rpc proxy listening");

    let sweep_state = Arc::clone(&state);
    let sweep_shutdown = Arc::clone(&shutdown);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let evicted = sweep_state.limiter.sweep(mono_now_ms());
                    if evicted > 0 {
                        tracing::debug!(evicted, "rpc proxy swept idle client buckets");
                    }
                }
                _ = sweep_shutdown.notified() => break,
            }
        }
    });

    let app = build_router(state);
    let graceful = shutdown.clone();
    let _ = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        graceful.notified().await;
    })
    .await;
}

fn json_error(
    status: axum::http::StatusCode,
    code: i64,
    message: &str,
) -> axum::response::Response {
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {"code": code, "message": message},
    });
    (
        status,
        [(CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Forwards a JSON-RPC POST to the upstream behind the rate limit.
async fn handler(
    axum::extract::State(state): axum::extract::State<Arc<ProxyState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: axum::body::Body,
) -> axum::response::Response {
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;

    let started = std::time::Instant::now();

    // Body size first: oversized payloads are dropped before rate accounting
    // so a large-body flood cannot spend budget it should never get.
    let bytes = match axum::body::to_bytes(body, state.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => {
            return json_error(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                -32600,
                "request body too large",
            );
        }
    };

    let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let client = client_ip(peer, xff, &state.trusted_proxies);
    if let Err(retry_ms) = state.limiter.check(client, mono_now_ms()) {
        state.metrics.rpc_proxy_limited_total.inc();
        let retry_after = retry_ms.div_ceil(1000).max(1);
        let mut response = json_error(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            -32029,
            &format!("rate limited: retry after {retry_after}s"),
        );
        if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert("retry-after", v);
        }
        return response;
    }

    // Bound concurrent upstream work: beyond `max_in_flight`, requests shed
    // with 503 immediately instead of queueing unboundedly against a slow
    // Reth.
    let _permit = match Arc::clone(&state.in_flight).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return json_error(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                -32000,
                "server busy",
            );
        }
    };

    let url = format!(
        "{}{}",
        state.upstream.trim_end_matches('/'),
        uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
    );
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let mut request = state
        .http
        .post(&url)
        .header(CONTENT_TYPE, content_type)
        .header("x-forwarded-for", client.to_string())
        .body(bytes);
    if let Some(auth) = headers.get("authorization") {
        request = request.header("authorization", auth);
    }

    let result = request.send().await;
    state.metrics.rpc_proxy_requests_total.inc();
    state
        .metrics
        .rpc_proxy_request_duration
        .observe(started.elapsed().as_secs_f64());

    let response = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(client = %client, error = %e, "rpc proxy upstream request failed");
            return json_error(
                axum::http::StatusCode::BAD_GATEWAY,
                -32603,
                "upstream unavailable",
            );
        }
    };

    let status = axum::http::StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    match response.bytes().await {
        Ok(body) => (
            status,
            [(CONTENT_TYPE, content_type)],
            axum::body::Body::from(body),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(client = %client, error = %e, "rpc proxy upstream body read failed");
            json_error(
                axum::http::StatusCode::BAD_GATEWAY,
                -32603,
                "upstream connection broken",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn peer(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::from(ip), port))
    }

    fn limiter(capacity: f64, rps: f64) -> RateLimiter {
        RateLimiter::new(capacity, rps)
    }

    // --- TokenBucket ---

    #[test]
    fn burst_allows_capacity_then_denies() {
        let mut b = TokenBucket::new(3.0, 0.001);
        assert!(b.try_take(100).is_ok());
        assert!(b.try_take(100).is_ok());
        assert!(b.try_take(100).is_ok());
        assert!(b.try_take(100).is_err());
    }

    #[test]
    fn refill_admits_one_token_after_rate_elapsed() {
        let mut b = TokenBucket::new(1.0, 0.5); // 1 token per 2 ms
        assert!(b.try_take(0).is_ok());
        assert!(b.try_take(1).is_err());
        assert!(b.try_take(2).is_ok());
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let mut b = TokenBucket::new(2.0, 0.5);
        // 1000 ms of idle would refill 500 tokens; the bucket must stay at 2.
        assert!(b.try_take(1000).is_ok());
        assert!(b.try_take(1000).is_ok());
        assert!(b.try_take(1000).is_err());
    }

    #[test]
    fn backwards_clock_clamps_to_no_refill() {
        let mut b = TokenBucket::new(1.0, 1.0);
        assert!(b.try_take(1000).is_ok());
        assert!(b.try_take(999).is_err());
        assert!(b.try_take(999).is_err());
    }

    #[test]
    fn denied_wait_is_time_to_next_token() {
        let mut b = TokenBucket::new(1.0, 0.5); // one token per 2 ms
        assert!(b.try_take(0).is_ok());
        // One ms of refill banked half a token; a full token needs one more.
        assert_eq!(b.try_take(1), Err(1));
        // With no elapsed time at all, zero tokens are banked and a full
        // token is two ms away.
        let mut c = TokenBucket::new(1.0, 0.5);
        assert!(c.try_take(0).is_ok());
        assert_eq!(c.try_take(0), Err(2));
    }

    // --- RateLimiter ---

    #[test]
    fn buckets_are_per_client() {
        let l = limiter(1.0, 0.0);
        let now = mono_now_ms();
        assert!(l.check(peer([1, 0, 0, 1], 1).ip(), now).is_ok());
        assert!(l.check(peer([1, 0, 0, 1], 1).ip(), now).is_err());
        assert!(l.check(peer([1, 0, 0, 2], 1).ip(), now).is_ok());
    }

    #[test]
    fn denied_retry_after_is_positive() {
        // 1 request per second: the next token is 1000 ms away.
        let l = limiter(1.0, 1.0);
        let now = mono_now_ms();
        let client = peer([2, 0, 0, 1], 1).ip();
        assert!(l.check(client, now).is_ok());
        match l.check(client, now) {
            Err(ms) => assert!((1..=1000).contains(&ms)),
            Ok(()) => panic!("expected denial"),
        }
    }

    #[test]
    fn sweep_evicts_idle_and_keeps_fresh() {
        let l = limiter(2.0, 0.0);
        let now = mono_now_ms();
        let stale = peer([3, 0, 0, 1], 1).ip();
        let fresh = peer([3, 0, 0, 2], 1).ip();
        assert!(l.check(stale, now).is_ok());
        assert!(l.check(fresh, now).is_ok());
        assert_eq!(l.len(), 2);
        let later = now + IDLE_MS + 1;
        // Re-touching fresh at the later time resets its idle clock; stale
        // stays untouched and is the only eviction.
        assert!(l.check(fresh, later).is_ok());
        assert_eq!(l.sweep(later), 1);
        assert_eq!(l.len(), 1);
        // A second sweep evicts nothing: fresh's clock was reset.
        assert_eq!(l.sweep(later), 0);
    }

    #[test]
    fn new_keys_are_rejected_when_the_map_is_at_cap() {
        let mut l = limiter(10.0, 0.0);
        l.max_keys = 2;
        let now = mono_now_ms();
        assert!(l.check(peer([4, 0, 0, 1], 1).ip(), now).is_ok());
        assert!(l.check(peer([4, 0, 0, 2], 1).ip(), now).is_ok());
        assert!(l.check(peer([4, 0, 0, 3], 1).ip(), now).is_err());
    }

    #[test]
    fn existing_keys_still_work_at_cap() {
        let mut l = limiter(10.0, 0.0);
        l.max_keys = 2;
        let now = mono_now_ms();
        let known = peer([5, 0, 0, 1], 1).ip();
        assert!(l.check(known, now).is_ok());
        assert!(l.check(peer([5, 0, 0, 2], 1).ip(), now).is_ok());
        assert!(l.check(known, now).is_ok());
        assert!(l.check(peer([5, 0, 0, 3], 1).ip(), now).is_err());
    }

    // --- client_ip ---

    #[test]
    fn untrusted_peer_ignores_xff() {
        let trusted: Vec<IpAddr> = vec![];
        let p = peer([9, 9, 9, 9], 5000);
        assert_eq!(
            client_ip(p, Some("1.2.3.4"), &trusted),
            peer([9, 9, 9, 9], 0).ip()
        );
    }

    #[test]
    fn trusted_peer_uses_rightmost_non_trusted_xff() {
        let trusted: Vec<IpAddr> = vec![peer([10, 0, 0, 1], 0).ip()];
        let p = peer([10, 0, 0, 1], 5000);
        assert_eq!(
            client_ip(p, Some("6.6.6.6, 7.7.7.7"), &trusted),
            peer([7, 7, 7, 7], 0).ip()
        );
    }

    #[test]
    fn xff_walk_skips_intermediate_trusted_hops() {
        let trusted: Vec<IpAddr> = vec![peer([10, 0, 0, 1], 0).ip(), peer([10, 0, 0, 2], 0).ip()];
        let p = peer([10, 0, 0, 1], 5000);
        assert_eq!(
            client_ip(p, Some("1.2.3.4, 10.0.0.2"), &trusted),
            peer([1, 2, 3, 4], 0).ip()
        );
    }

    #[test]
    fn all_trusted_chain_falls_back_to_peer() {
        let trusted: Vec<IpAddr> = vec![peer([10, 0, 0, 1], 0).ip(), peer([10, 0, 0, 2], 0).ip()];
        let p = peer([10, 0, 0, 1], 5000);
        assert_eq!(
            client_ip(p, Some("10.0.0.2"), &trusted),
            peer([10, 0, 0, 1], 0).ip()
        );
    }

    #[test]
    fn missing_xff_from_trusted_peer_falls_back_to_peer() {
        let trusted: Vec<IpAddr> = vec![peer([10, 0, 0, 1], 0).ip()];
        let p = peer([10, 0, 0, 1], 5000);
        assert_eq!(client_ip(p, None, &trusted), peer([10, 0, 0, 1], 0).ip());
    }

    #[test]
    fn malformed_xff_entries_are_skipped() {
        let trusted: Vec<IpAddr> = vec![peer([10, 0, 0, 1], 0).ip()];
        let p = peer([10, 0, 0, 1], 5000);
        assert_eq!(
            client_ip(p, Some("not-an-ip, 8.8.8.8"), &trusted),
            peer([8, 8, 8, 8], 0).ip()
        );
    }

    // --- RateLimits ---

    #[test]
    fn rate_limits_accept_normal_values() {
        let rl = RateLimits::from_config(10.0, 25).expect("valid");
        assert_eq!(rl.capacity, 25.0);
        assert_eq!(rl.refill_per_ms, 0.01);
    }

    #[test]
    fn rate_limits_reject_zero_rate() {
        assert!(RateLimits::from_config(0.0, 25).is_err());
    }

    #[test]
    fn rate_limits_reject_zero_burst() {
        assert!(RateLimits::from_config(10.0, 0).is_err());
    }

    #[test]
    fn rate_limits_reject_negative_rate() {
        assert!(RateLimits::from_config(-1.0, 25).is_err());
    }

    // --- HTTP layer (integration) ---

    use crate::metrics::PodseqMetrics;

    /// A request the stub upstream fully received.
    #[derive(Debug, Clone)]
    struct RecordedRequest {
        path: String,
        content_type: String,
        xff: Option<String>,
        auth: Option<String>,
        body: Vec<u8>,
    }

    /// The canned reply the stub sends after recording a request.
    #[derive(Debug, Clone)]
    struct StubReply {
        status: u16,
        body: Vec<u8>,
    }

    impl Default for StubReply {
        fn default() -> Self {
            Self {
                status: 200,
                body: br#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#.to_vec(),
            }
        }
    }

    /// Handle over a hand-rolled HTTP/1.1 stub upstream: it records every
    /// fully-read request on an mpsc and answers with the current
    /// [`StubReply`]. With `hold`, it answers only after `release` fires.
    struct StubUpstream {
        addr: SocketAddr,
        requests: tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
        reply: Arc<Mutex<StubReply>>,
        release: Arc<tokio::sync::Notify>,
    }

    impl StubUpstream {
        fn set_reply(&self, status: u16, body: &[u8]) {
            *self.reply.lock().expect("stub reply lock") = StubReply {
                status,
                body: body.to_vec(),
            };
        }

        fn try_recv(&mut self) -> Option<RecordedRequest> {
            self.requests.try_recv().ok()
        }
    }

    async fn spawn_stub(hold: bool) -> StubUpstream {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let reply = Arc::new(Mutex::new(StubReply::default()));
        let release = Arc::new(tokio::sync::Notify::new());
        let (reply_shared, release_shared) = (Arc::clone(&reply), Arc::clone(&release));

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
                let reply = Arc::clone(&reply_shared);
                let release = Arc::clone(&release_shared);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read until end of headers.
                    let header_end;
                    loop {
                        let Ok(n) = sock.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = find_header_end(&buf) {
                            header_end = pos;
                            break;
                        }
                    }
                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_length = headers
                        .lines()
                        .find_map(|l| {
                            let (name, value) = l.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    // Read the body to its declared length.
                    while buf.len() < header_end + 4 + content_length {
                        let Ok(n) = sock.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = buf[header_end + 4..].to_vec();
                    let path = headers
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let header_value = |name: &str| {
                        headers.lines().find_map(|l| {
                            let (n, v) = l.split_once(':')?;
                            n.trim()
                                .eq_ignore_ascii_case(name)
                                .then(|| v.trim().to_string())
                        })
                    };
                    let recorded = RecordedRequest {
                        path,
                        content_type: header_value("content-type").unwrap_or_default(),
                        xff: header_value("x-forwarded-for"),
                        auth: header_value("authorization"),
                        body,
                    };
                    let _ = tx.send(recorded);
                    if hold {
                        release.notified().await;
                    }
                    let reply = reply.lock().expect("stub reply lock").clone();
                    let response = format!(
                        "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.write_all(&reply.body).await;
                    let _ = sock.flush().await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        StubUpstream {
            addr,
            requests: rx,
            reply,
            release,
        }
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn test_state(
        upstream: &str,
        capacity: f64,
        requests_per_second: f64,
        max_body_bytes: usize,
        max_in_flight: usize,
        trusted: Vec<IpAddr>,
    ) -> Arc<ProxyState> {
        Arc::new(
            ProxyState::new(
                RateLimits {
                    capacity,
                    refill_per_ms: requests_per_second / 1000.0,
                },
                upstream.to_string(),
                max_body_bytes,
                max_in_flight,
                trusted,
                Arc::new(PodseqMetrics::new()),
            )
            .expect("valid proxy state"),
        )
    }

    fn test_state_with_metrics(
        upstream: &str,
        capacity: f64,
        requests_per_second: f64,
        metrics: Arc<PodseqMetrics>,
    ) -> (Arc<ProxyState>, Arc<PodseqMetrics>) {
        let state = Arc::new(
            ProxyState::new(
                RateLimits {
                    capacity,
                    refill_per_ms: requests_per_second / 1000.0,
                },
                upstream.to_string(),
                1024 * 1024,
                256,
                Vec::new(),
                Arc::clone(&metrics),
            )
            .expect("valid proxy state"),
        );
        (state, metrics)
    }

    /// Binds the proxy router on an ephemeral port and serves it on a task.
    async fn spawn_proxy(state: Arc<ProxyState>) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_router(state);
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        addr
    }

    async fn post(
        client: &reqwest::Client,
        addr: SocketAddr,
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut req = client
            .post(format!("http://{addr}/"))
            .header("content-type", "application/json")
            .body(body.to_vec());
        for (name, value) in extra_headers {
            req = req.header(*name, *value);
        }
        req.send().await.expect("proxy request completes")
    }

    const ETH_CALL_BODY: &[u8] =
        br#"{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}"#;

    #[tokio::test]
    async fn forwards_post_to_upstream() {
        let mut stub = spawn_stub(false).await;
        let addr = spawn_proxy(test_state(
            &format!("http://{}", stub.addr),
            100.0,
            1000.0,
            1024 * 1024,
            256,
            Vec::new(),
        ))
        .await;

        let client = reqwest::Client::new();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &client,
                addr,
                ETH_CALL_BODY,
                &[("authorization", "Bearer test")],
            ),
        )
        .await
        .expect("no hang");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.bytes().await.unwrap().as_ref(),
            br#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#
        );

        let recorded = stub.try_recv().expect("upstream saw the request");
        assert_eq!(recorded.path, "/");
        assert_eq!(recorded.content_type, "application/json");
        assert_eq!(recorded.body, ETH_CALL_BODY);
        // The caller's authorization rides through to the upstream.
        assert_eq!(recorded.auth.as_deref(), Some("Bearer test"));
        // Direct peer, no trusted proxies: the forwarded identity is the
        // loopback peer address, not anything the client may have sent.
        assert_eq!(recorded.xff.as_deref(), Some("127.0.0.1"));
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_before_the_upstream() {
        let mut stub = spawn_stub(false).await;
        let addr = spawn_proxy(test_state(
            &format!("http://{}", stub.addr),
            100.0,
            1000.0,
            16,
            256,
            Vec::new(),
        ))
        .await;

        let client = reqwest::Client::new();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            post(&client, addr, &[b'x'; 32], &[]),
        )
        .await
        .expect("no hang");
        assert_eq!(response.status(), 413);
        let text = response.text().await.unwrap();
        assert!(text.contains("request body too large"));
        assert!(stub.try_recv().is_none(), "upstream must not see it");
    }

    #[tokio::test]
    async fn limited_requests_get_429_with_retry_after() {
        let stub = spawn_stub(false).await;
        let addr = spawn_proxy(test_state(
            &format!("http://{}", stub.addr),
            2.0,
            1.0,
            1024 * 1024,
            256,
            Vec::new(),
        ))
        .await;

        let client = reqwest::Client::new();
        for _ in 0..2 {
            let response = tokio::time::timeout(
                Duration::from_secs(5),
                post(&client, addr, ETH_CALL_BODY, &[]),
            )
            .await
            .expect("no hang");
            assert_eq!(response.status(), 200);
        }
        let limited = tokio::time::timeout(
            Duration::from_secs(5),
            post(&client, addr, ETH_CALL_BODY, &[]),
        )
        .await
        .expect("no hang");
        assert_eq!(limited.status(), 429);
        let retry_after: u64 = limited
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .expect("retry-after header present");
        assert!(retry_after >= 1);
        let text = limited.text().await.unwrap();
        assert!(text.contains("-32029"));
        assert!(text.contains("rate limited"));
    }

    #[tokio::test]
    async fn trusted_proxy_xff_gives_per_client_buckets() {
        let stub = spawn_stub(false).await;
        let trusted = vec!["127.0.0.1".parse::<IpAddr>().unwrap()];
        let addr = spawn_proxy(test_state(
            &format!("http://{}", stub.addr),
            1.0,
            0.0,
            1024 * 1024,
            256,
            trusted,
        ))
        .await;

        let client = reqwest::Client::new();
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &client,
                addr,
                ETH_CALL_BODY,
                &[("x-forwarded-for", "1.1.1.1")],
            ),
        )
        .await
        .expect("no hang");
        assert_eq!(first.status(), 200, "fresh client 1.1.1.1 is admitted");

        let other = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &client,
                addr,
                ETH_CALL_BODY,
                &[("x-forwarded-for", "2.2.2.2")],
            ),
        )
        .await
        .expect("no hang");
        assert_eq!(other.status(), 200, "fresh client 2.2.2.2 is admitted");

        let limited = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &client,
                addr,
                ETH_CALL_BODY,
                &[("x-forwarded-for", "1.1.1.1")],
            ),
        )
        .await
        .expect("no hang");
        assert_eq!(limited.status(), 429, "exhausted client 1.1.1.1 is limited");
    }

    #[tokio::test]
    async fn untrusted_client_xff_is_ignored() {
        let stub = spawn_stub(false).await;
        let addr = spawn_proxy(test_state(
            &format!("http://{}", stub.addr),
            1.0,
            0.0,
            1024 * 1024,
            256,
            Vec::new(),
        ))
        .await;

        let client = reqwest::Client::new();
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &client,
                addr,
                ETH_CALL_BODY,
                &[("x-forwarded-for", "1.1.1.1")],
            ),
        )
        .await
        .expect("no hang");
        assert_eq!(first.status(), 200);

        // A different forged XFF is the same direct peer: same bucket, 429.
        let limited = tokio::time::timeout(
            Duration::from_secs(5),
            post(
                &client,
                addr,
                ETH_CALL_BODY,
                &[("x-forwarded-for", "2.2.2.2")],
            ),
        )
        .await
        .expect("no hang");
        assert_eq!(limited.status(), 429);
    }

    #[tokio::test]
    async fn upstream_error_status_passes_through() {
        let stub = spawn_stub(false).await;
        stub.set_reply(500, br#"{"error":"boom"}"#);
        let addr = spawn_proxy(test_state(
            &format!("http://{}", stub.addr),
            100.0,
            1000.0,
            1024 * 1024,
            256,
            Vec::new(),
        ))
        .await;

        let client = reqwest::Client::new();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            post(&client, addr, ETH_CALL_BODY, &[]),
        )
        .await
        .expect("no hang");
        assert_eq!(response.status(), 500);
        assert_eq!(
            response.bytes().await.unwrap().as_ref(),
            br#"{"error":"boom"}"#
        );
    }

    #[tokio::test]
    async fn unreachable_upstream_yields_502() {
        // Bind and drop a listener to claim a port nothing answers on.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let addr = spawn_proxy(test_state(
            &format!("http://{dead_addr}"),
            100.0,
            1000.0,
            1024 * 1024,
            256,
            Vec::new(),
        ))
        .await;

        let client = reqwest::Client::new();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            post(&client, addr, ETH_CALL_BODY, &[]),
        )
        .await
        .expect("no hang");
        assert_eq!(response.status(), 502);
        let text = response.text().await.unwrap();
        assert!(text.contains("upstream unavailable"));
    }

    #[tokio::test]
    async fn in_flight_cap_sheds_until_the_upstream_drains() {
        let stub = spawn_stub(true).await;
        let addr = spawn_proxy(test_state(
            &format!("http://{}", stub.addr),
            100.0,
            1000.0,
            1024 * 1024,
            1, // single in-flight slot
            Vec::new(),
        ))
        .await;

        // First request: occupies the slot, parked at the stub.
        let client1 = reqwest::Client::new();
        let parked = tokio::spawn(
            client1
                .post(format!("http://{addr}/"))
                .header("content-type", "application/json")
                .body(ETH_CALL_BODY.to_vec())
                .send(),
        );
        // Wait until the stub has actually received it.
        let mut stub_requests = stub.requests;
        let _ = tokio::time::timeout(Duration::from_secs(5), stub_requests.recv())
            .await
            .expect("stub received the parked request");

        // Second request: no slot left, shed with 503.
        let client2 = reqwest::Client::new();
        let shed = tokio::time::timeout(
            Duration::from_secs(5),
            post(&client2, addr, ETH_CALL_BODY, &[]),
        )
        .await
        .expect("no hang");
        assert_eq!(shed.status(), 503);
        assert!(shed.text().await.unwrap().contains("server busy"));

        // Drain: the parked request completes with the stub's answer.
        stub.release.notify_one();
        let parked_response = tokio::time::timeout(Duration::from_secs(5), parked)
            .await
            .expect("no hang")
            .expect("join ok")
            .expect("send ok");
        assert_eq!(parked_response.status(), 200);
    }

    #[tokio::test]
    async fn proxy_metrics_record_forward_and_limit() {
        let stub = spawn_stub(false).await;
        let metrics = Arc::new(PodseqMetrics::new());
        let (state, _metrics) = test_state_with_metrics(
            &format!("http://{}", stub.addr),
            1.0,
            0.0,
            Arc::clone(&metrics),
        );
        let addr = spawn_proxy(state).await;

        let client = reqwest::Client::new();
        let ok = tokio::time::timeout(
            Duration::from_secs(5),
            post(&client, addr, ETH_CALL_BODY, &[]),
        )
        .await
        .expect("no hang");
        assert_eq!(ok.status(), 200);
        let limited = tokio::time::timeout(
            Duration::from_secs(5),
            post(&client, addr, ETH_CALL_BODY, &[]),
        )
        .await
        .expect("no hang");
        assert_eq!(limited.status(), 429);

        let mut buffer = String::new();
        prometheus_client::encoding::text::encode(&mut buffer, &metrics.registry).unwrap();
        // Counters encode with an appended `_total` on the sample line.
        assert!(buffer.contains("podseq_rpc_proxy_requests_total 1"));
        assert!(buffer.contains("podseq_rpc_proxy_limited_total 1"));
        assert!(buffer.contains("# TYPE podseq_rpc_proxy_request_duration_seconds histogram"));
    }

    #[test]
    fn proxy_state_rejects_bad_configuration() {
        let metrics = Arc::new(PodseqMetrics::new());
        let ok_limits = RateLimits {
            capacity: 1.0,
            refill_per_ms: 0.001,
        };
        assert!(ProxyState::new(
            ok_limits.clone(),
            "http://127.0.0.1:8545".into(),
            0,
            256,
            Vec::new(),
            Arc::clone(&metrics)
        )
        .is_err());
        assert!(ProxyState::new(
            ok_limits.clone(),
            "http://127.0.0.1:8545".into(),
            1024,
            0,
            Vec::new(),
            Arc::clone(&metrics)
        )
        .is_err());
        assert!(ProxyState::new(
            ok_limits.clone(),
            "not a url".into(),
            1024,
            256,
            Vec::new(),
            Arc::clone(&metrics)
        )
        .is_err());
        assert!(ProxyState::new(
            ok_limits,
            "http://127.0.0.1:8545".into(),
            1024,
            256,
            Vec::new(),
            metrics
        )
        .is_ok());
    }
}
