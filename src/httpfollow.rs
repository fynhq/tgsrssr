//! Manual redirect following with per-hop SSRF checks.

use anyhow::Context;

use crate::state::AppState;

pub(crate) const MAX_REDIRECTS: u32 = 5;

#[derive(Clone, Copy)]
pub(crate) enum HopPolicy<'a> {
    // Feed/media: documented target policy, with the trusted-private allowlist scoped to the origin.
    Trusted {
        allow: bool,
        origin: Option<&'a str>,
    },
    // Platform APIs: literal check always, DNS classification only on direct routes.
    Api,
}

#[derive(Clone, Copy)]
pub(crate) enum RedirectPolicy {
    Follow(u32),
    // Hand the 3xx response back so the caller can detect login-page redirects.
    Return,
}

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
    policy: HopPolicy<'_>,
    scope: &'static str,
    redirects: RedirectPolicy,
    mut build_request: F,
) -> anyhow::Result<reqwest::Response>
where
    F: FnMut(&reqwest::Client, &str, u32) -> reqwest::RequestBuilder,
{
    let mut cur_url = start_url.to_string();
    let mut hops = 0u32;
    loop {
        let client = match policy {
            HopPolicy::Trusted { allow, origin } => {
                let allow_trusted =
                    allow && origin.is_none_or(|origin| same_origin(&cur_url, origin));
                state.check_target(&cur_url, allow_trusted, scope).await?;
                state.media_client_for_url_trusted(&cur_url, allow_trusted)
            }
            HopPolicy::Api => {
                state.check_api_target(&cur_url).await?;
                state.media_client_for_url(&cur_url)
            }
        };
        let resp = build_request(client, &cur_url, hops)
            .send()
            .await
            .with_context(|| format!("request failed: {}", cur_url))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(resp);
        }
        if resp.status().is_redirection() {
            let RedirectPolicy::Follow(max_redirects) = redirects else {
                return Ok(resp);
            };
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
