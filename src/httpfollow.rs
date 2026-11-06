//! Manual redirect following shared by the feed fetch (fetcher.rs) and the
//! media download (media.rs) paths.
//!
//! Both paths disable client auto-redirects and must re-run the SSRF target
//! policy on every hop; the loop shape (Location parsing, hop cap, error
//! copy) was duplicated verbatim before this module existed.

use anyhow::Context;

use crate::state::AppState;

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
        // Substring contract: "dns resolve failed" (produced on the direct
        // route) is matched by sendpolicy::is_net_error as a transient
        // network error.
        state.check_target(&cur_url, allow_trusted_private, scope).await?;
        let client = state.media_client_for_url_trusted(&cur_url, allow_trusted_private);
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
