//! Manual redirect following with per-hop SSRF checks.

use anyhow::Context;

use crate::state::AppState;

// The trusted-private-host bypass must not survive a redirect or embedded URL to another authority.
pub(crate) fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(ua), Ok(ub)) = (url::Url::parse(a), url::Url::parse(b)) else {
        return false;
    };
    ua.scheme() == ub.scheme()
        && ua.host_str() == ub.host_str()
        && ua.port_or_known_default() == ub.port_or_known_default()
}

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
        let allow_trusted = allow_trusted_private
            && trusted_origin.is_none_or(|origin| same_origin(&cur_url, origin));
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
