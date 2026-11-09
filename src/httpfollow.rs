//! Manual redirect following shared by the feed fetch (fetcher.rs) and the
//! media download (media.rs) paths.
//!
//! Both paths disable client auto-redirects and must re-run the SSRF target
//! policy on every hop; the loop shape (Location parsing, hop cap, error
//! copy) was duplicated verbatim before this module existed.

use anyhow::Context;

use crate::state::AppState;

/// Same-origin check (scheme + host + effective port). Used to keep the
/// RSS_TRUSTED_PRIVATE_HOSTS bypass scoped to media served by the feed
/// itself: a public feed embedding `http://rsshub:1200/...` must not inherit
/// the admin's trusted-host bypass, including after a redirect.
pub(crate) fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(ua), Ok(ub)) = (url::Url::parse(a), url::Url::parse(b)) else {
        return false;
    };
    ua.scheme() == ub.scheme()
        && ua.host_str() == ub.host_str()
        && ua.port_or_known_default() == ub.port_or_known_default()
}

/// Manually follow up to `max_redirects` hops on a GET-like request.
///
/// `build_request` runs for every hop with the client selected for that
/// hop's URL and the hop index (0 = the original request), so callers can
/// attach per-hop headers while keeping one-shot headers (conditional
/// requests) on the first hop only. The target policy is re-checked before
/// each request, including the first, against the URL actually requested —
/// a direct URL cannot redirect to a private literal IP / private DNS
/// answer with the checks skipped.
///
/// A 304 Not Modified is returned as-is: it is a 3xx status but not a
/// redirect, and treating it as one falsely reports a missing Location
/// header.
pub(crate) async fn get_follow_redirects<F>(
    state: &AppState,
    start_url: &str,
    allow_trusted_private: bool,
    trusted_origin: Option<&str>,
    scope: &'static str,
    max_redirects: u32,
    mut build_request: F,
) -> anyhow::Result<reqwest::Response>
where
    F: FnMut(&reqwest::Client, &str, u32) -> reqwest::RequestBuilder,
{
    let mut cur_url = start_url.to_string();
    let mut hops = 0u32;
    loop {
        // The trusted-private bypass is per-hop and, when an origin is given,
        // limited to that origin. A redirect from a trusted origin to a
        // different (even allowlisted) host is re-checked as normal traffic.
        let allow_trusted = allow_trusted_private
            && trusted_origin.is_none_or(|origin| same_origin(&cur_url, origin));
        // Substring contract: "dns resolve failed" (produced on the direct
        // route) is matched by sendpolicy::is_net_error as a transient
        // network error.
        state.check_target(&cur_url, allow_trusted, scope).await?;
        let client = state.media_client_for_url_trusted(&cur_url, allow_trusted);
        let resp = build_request(client, &cur_url, hops)
            .send()
            .await
            .with_context(|| format!("request failed: {}", cur_url))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(resp);
        }
        if resp.status().is_redirection() {
            hops += 1;
            if hops > max_redirects {
                anyhow::bail!("too many redirects (>{}): {}", max_redirects, start_url);
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("redirect without Location header"))?;
            let next = url::Url::parse(&cur_url)
                .and_then(|base| base.join(loc))
                .map_err(|e| anyhow::anyhow!("bad redirect Location: {}", e))?;
            cur_url = next.to_string();
            continue;
        }
        return Ok(resp);
    }
}
