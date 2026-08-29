//! Shared HTTP client construction.
//!
//! Every browser-process fetch in buffr uses the same policy: bounded
//! timeouts, the buffr user-agent, and **no redirects**. The redirect-off
//! part is load-bearing (audit §17-1/§16-2): the callers gate the URL they
//! fetch, and a 3xx followed to a loopback/RFC1918 hop would be fetched with
//! no re-validation. Centralising the agent keeps a future fourth fetch from
//! silently re-inheriting ureq's default redirect-following behaviour.

use std::time::Duration;

/// The user-agent every buffr HTTP request carries.
pub const USER_AGENT: &str = concat!("buffr/", env!("CARGO_PKG_VERSION"));

/// Build a ureq agent with buffr's network policy: `connect_timeout` /
/// `recv_timeout` bounds, the buffr UA, and `max_redirects(0)` so a 3xx
/// comes back as-is for the caller to handle.
pub fn agent(connect_timeout: Duration, recv_timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(connect_timeout))
        .timeout_recv_response(Some(recv_timeout))
        .user_agent(USER_AGENT)
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}
