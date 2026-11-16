//! Telegram public-channel native fetcher.

use std::sync::LazyLock;

use anyhow::Context;
use chrono::{DateTime, Utc};
use feed_rs::model::Entry;

use crate::fetcher::FetchOptions;
use crate::i18n::{self, Lang};
use crate::state::AppState;

pub fn is_telegram_url(url: &str) -> bool {
    parse_channel_name(url).is_some()
}

pub fn parse_channel_name(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_lowercase();
    if !matches!(host.as_str(), "t.me" | "telegram.me") {
        return None;
    }
    let segs: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    let name = if segs[0] == "s" {
        *segs.get(1)?
    } else {
        segs[0]
    };
    const RESERVED: &[&str] = &[
        "s",
        "i",
        "c",
        "joinchat",
        "share",
        "contact",
        "login",
        "setlanguage",
        "confirmphone",
        "addstickers",
        "addemoji",
        "addtheme",
        "addlist",
        "proxy",
        "socks",
        "boost",
        "gift",
        "gifts",
        "username",
    ];
    let name_lower = name.to_ascii_lowercase();
    if RESERVED.contains(&name_lower.as_str()) {
        return None;
    }
    if name.starts_with('+') {
        return None;
    }
    if name.len() < 5
        || name.len() > 32
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(name.to_string())
}

static RE_CHAN_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)tgme_channel_info_header_title[^>]*>.*?<span[^>]*>(.*?)</span>"#)
        .unwrap()
});
static RE_DATA_POST: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"data-post="([^"]+)""#).unwrap());
static RE_MSG_TEXT_OPEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<div[^>]*tgme_widget_message_text js-message_text[^>]*>"#).unwrap()
});
static RE_REPLY_BLOCK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<a[^>]*tgme_widget_message_reply[^>]*>"#).unwrap());
static RE_REPLY_HREF: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)\bhref="([^"]*)""#).unwrap());
static RE_REPLY_AUTHOR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)tgme_widget_message_author_name[^>]*>(.*?)</span>"#).unwrap()
});
static RE_REPLY_TEXT_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?is)<div[^>]*js-message_reply_text[^>]*>"#).unwrap());
static RE_PHOTO_BG: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?is)<a\b[^>]*tgme_widget_message_photo_wrap[^>]*background-image:url\('([^']+)'\)"#,
    )
    .unwrap()
});
static RE_VIDEO_SRC: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<video\b[^>]*?\bsrc\s*=\s*["']([^"']+)["']"#).unwrap()
});
static RE_MSG_POLL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_poll").unwrap());
static RE_MSG_DOCUMENT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_document").unwrap());
static RE_MSG_AUDIO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_(?:audio|voice)").unwrap());
static RE_MSG_STICKER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)tgme_widget_message_sticker").unwrap());
static RE_DOC_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)tgme_widget_message_document_title[^>]*>(.*?)</span>"#).unwrap()
});
static RE_DATETIME: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"datetime="([^"]+)""#).unwrap());
static RE_DATE_CONTAINER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<[a-z]+[^>]*tgme_widget_message_date[^>]*>"#).unwrap()
});
static RE_ANCHOR_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<a\b").unwrap());
static RE_ANCHOR_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</a\s*>").unwrap());
static RE_DIV_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<div\b").unwrap());
static RE_DIV_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</div\s*>").unwrap());

pub async fn fetch_by_url(
    state: &AppState,
    url: String,
    opts: &FetchOptions<'_>,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    let channel = parse_channel_name(&url)
        .ok_or_else(|| anyhow::anyhow!("Invalid Telegram channel URL: {}", url))?;
    fetch_telegram_channel(state, &channel, opts.lang).await
}

pub async fn fetch_telegram_channel(
    state: &AppState,
    channel: &str,
    lang: Lang,
) -> anyhow::Result<crate::fetcher::FetchResult> {
    const MAX_PAGES: usize = 3;
    let base_url = format!("https://t.me/s/{}", channel);
    let mut entries: Vec<Entry> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut feed_title: Option<String> = None;
    let mut total_blocks = 0usize;
    let mut before: Option<String> = None;

    for page in 0..MAX_PAGES {
        if page > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
        let url = match &before {
            Some(id) => format!("{}?before={}", base_url, id),
            None => base_url.clone(),
        };
        let resp = match crate::platforms::api_get(
            state,
            &url,
            &[("Accept", "text/html,application/xhtml+xml".to_string())],
            5,
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) if page == 0 => {
                return Err(e).with_context(|| format!("Telegram channel request failed: {}", url));
            }
            Err(e) => {
                tracing::warn!(channel = %channel, url = %url, error = %e, "telegram page fetch failed; stopping pagination");
                break;
            }
        };

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            if page == 0 {
                anyhow::bail!(
                    "Telegram channel does not exist or is private (no public web preview): @{}",
                    channel
                );
            }
            break;
        }
        if !status.is_success() {
            if page == 0 {
                anyhow::bail!("Telegram channel preview HTTP {}", status);
            }
            tracing::warn!(channel = %channel, status = %status, "telegram page HTTP error; stopping pagination");
            break;
        }
        let ctx = format!("Telegram channel @{} page {}", channel, page + 1);
        let body_buf = crate::fetcher::read_body_capped(resp, &ctx).await?;
        let html = String::from_utf8_lossy(&body_buf).into_owned();

        if feed_title.is_none() {
            feed_title = extract_channel_title(&html).or_else(|| {
                tracing::warn!(
                    channel = %channel,
                    body_len = body_buf.len(),
                    "telegram channel title not found in preview page (falling back to channel slug; page structure may have changed)"
                );
                Some(channel.to_string())
            });
        }

        let blocks = split_message_blocks(&html);
        total_blocks += blocks.len();
        let before_len = entries.len();
        for (post, block) in &blocks {
            if let Some(e) =
                message_to_entry(post, block, feed_title.as_deref().unwrap_or(channel), lang)
            {
                crate::platforms::push_unique_entry(&mut entries, &mut seen_ids, e);
            }
        }

        let oldest = blocks
            .iter()
            .filter_map(|(post, _)| post.rsplit('/').next()?.parse::<i64>().ok())
            .min();
        if entries.len() == before_len {
            break;
        }
        match oldest {
            Some(id) if id > 1 => before = Some(id.to_string()),
            _ => break,
        }
    }
    let feed_title = feed_title.unwrap_or_else(|| channel.to_string());

    tracing::debug!(
        channel = %channel,
        blocks = total_blocks,
        entries = entries.len(),
        "telegram preview parsed"
    );

    crate::platforms::sort_entries_newest_first(&mut entries);

    // A page with message blocks but zero parsed entries must error, not return empty (silent feed death).
    if entries.is_empty() && total_blocks > 0 {
        tracing::warn!(
            channel = %channel,
            posts = total_blocks,
            "telegram preview has message blocks but zero entries extracted (page structure may have changed)"
        );
    }

    Ok(crate::fetcher::FetchResult::platform(
        Some(feed_title),
        entries,
    ))
}

fn split_message_blocks(html: &str) -> Vec<(String, &str)> {
    let posts: Vec<(usize, String)> = RE_DATA_POST
        .captures_iter(html)
        .filter_map(|c| {
            let m = c.get(0)?;
            let post = c[1].to_string();
            if post.contains('/') {
                Some((m.start(), post))
            } else {
                None
            }
        })
        .collect();
    posts
        .iter()
        .enumerate()
        .map(|(i, (start, post))| {
            let end = posts.get(i + 1).map(|(s, _)| *s).unwrap_or(html.len());
            (post.clone(), &html[*start..end])
        })
        .collect()
}

fn plausible_title(s: &str) -> bool {
    !s.is_empty()
        && crate::htmltext::utf16_len(s) <= 256
        && !s.contains('<')
        && !s.contains('>')
        && !s.contains("tgme_widget_message")
}

fn extract_channel_title(html: &str) -> Option<String> {
    let caps = RE_CHAN_TITLE.captures(html)?;
    let t = crate::fetcher::html_to_plain(&caps[1]);
    if !plausible_title(&t) {
        tracing::warn!(
            title_len = crate::htmltext::utf16_len(&t),
            "channel title extraction looks invalid; ignoring it"
        );
        return None;
    }
    Some(t)
}

fn message_to_entry(data_post: &str, block: &str, feed_title: &str, lang: Lang) -> Option<Entry> {
    let (_, msg_id) = data_post.rsplit_once('/')?;
    if msg_id.is_empty() {
        return None;
    }

    let published = extract_published(block);

    let main_html = extract_main_text_html(block, data_post);

    let reply_html = extract_reply_blockquote(block);

    let link = format!("https://t.me/{}", data_post);
    let mut body = String::new();
    if !reply_html.is_empty() {
        body.push_str(&reply_html);
        if !main_html.is_empty() {
            body.push('\n');
        }
    }
    body.push_str(&main_html);
    append_media_tags(&mut body, block);

    if body.trim().is_empty() {
        let marker: Option<String> = if RE_MSG_POLL.is_match(block) {
            Some("🗳️".to_string())
        } else if RE_MSG_DOCUMENT.is_match(block) {
            let name = RE_DOC_TITLE
                .captures(block)
                .map(|c| crate::fetcher::html_to_plain(&c[1]))
                .filter(|n| plausible_title(n))
                .unwrap_or_default();
            Some(if name.is_empty() {
                "📎".to_string()
            } else {
                format!("📎 {}", crate::htmltext::escape_html(&name))
            })
        } else if RE_MSG_AUDIO.is_match(block) {
            Some("🎧".to_string())
        } else if RE_MSG_STICKER.is_match(block) {
            Some("🖼️".to_string())
        } else {
            None
        };
        match marker {
            Some(m) => {
                body = format!(
                    "<a href=\"{}\">{}</a>",
                    crate::htmltext::escape_attr(&link),
                    m
                );
            }
            None => {
                tracing::debug!(post = %data_post, "telegram message skipped (service or unsupported)");
                return None;
            }
        }
    }

    let plain = crate::fetcher::html_to_plain(&main_html);
    let title = crate::platforms::make_title(&plain, i18n::t(lang, "telegram_media_title"));

    Some(crate::platforms::make_entry(
        msg_id.to_string(),
        title,
        feed_title.to_string(),
        body,
        "text/html",
        link,
        published,
    ))
}

fn extract_reply_blockquote(block: &str) -> String {
    let reply_caps = match RE_REPLY_BLOCK.captures(block) {
        Some(c) => c,
        None => return String::new(),
    };
    let href = reply_caps
        .get(0)
        .and_then(|m| RE_REPLY_HREF.captures(m.as_str()))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let open_end = reply_caps.get(0).map(|m| m.end()).unwrap_or(0);
    let inner = extract_balanced_anchor_inner(block, open_end);

    let author = RE_REPLY_AUTHOR
        .captures(&inner)
        .map(|c| crate::fetcher::html_to_plain(&c[1]).trim().to_string())
        .unwrap_or_default();

    let text = RE_REPLY_TEXT_OPEN
        .find(&inner)
        .map(|m| extract_balanced_div_inner(&inner, m.end()).0)
        .unwrap_or_default();

    if text.trim().is_empty() {
        return String::new();
    }

    let quoted = if author.is_empty() {
        text
    } else if href.is_empty() {
        format!("{}:\n{}", crate::htmltext::escape_html(&author), text)
    } else {
        format!(
            "<a href=\"{}\">{}</a>:\n{}",
            href,
            crate::htmltext::escape_html(&author),
            text
        )
    };
    format!("<blockquote>{}</blockquote>", quoted)
}

fn extract_published(block: &str) -> Option<DateTime<Utc>> {
    let reply_range = RE_REPLY_BLOCK.find(block).map(|reply| {
        let end = crate::htmltext::find_balanced_close(
            block,
            reply.end(),
            &RE_ANCHOR_OPEN,
            &RE_ANCHOR_CLOSE,
        )
        .map(|(close_start, _)| close_start)
        .unwrap_or(block.len());
        (reply.start(), end)
    });
    let in_reply = |pos: usize| matches!(&reply_range, Some((s, e)) if pos >= *s && pos < *e);

    for open in RE_DATE_CONTAINER.find_iter(block) {
        if in_reply(open.start()) {
            continue;
        }
        let s = &block[open.end()..];
        let s = match RE_DATA_POST.find(s) {
            Some(next) => &s[..next.start()],
            None => s,
        };
        let dt = RE_DATETIME
            .captures(s)
            .and_then(|c| DateTime::parse_from_rfc3339(&c[1]).ok())
            .map(|d| d.with_timezone(&Utc));
        if dt.is_some() {
            return dt;
        }
    }
    for caps in RE_DATETIME.captures_iter(block) {
        if caps.get(0).map(|m| in_reply(m.start())).unwrap_or(true) {
            continue;
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(&caps[1]) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    None
}

fn extract_main_text_html(block: &str, data_post: &str) -> String {
    let Some(open) = RE_MSG_TEXT_OPEN.find(block) else {
        return String::new();
    };
    let (inner, balanced) = extract_balanced_div_inner(block, open.end());
    if !balanced {
        tracing::warn!(
            post = %data_post,
            "telegram message text div has no balanced </div>; body may be truncated (page structure may have changed)"
        );
    }
    inner
}

fn extract_balanced_anchor_inner(html: &str, start: usize) -> String {
    match crate::htmltext::find_balanced_close(html, start, &RE_ANCHOR_OPEN, &RE_ANCHOR_CLOSE) {
        Some((close_start, _)) => html[start..close_start].to_string(),
        None => html[start..].to_string(),
    }
}

fn extract_balanced_div_inner(html: &str, start: usize) -> (String, bool) {
    match crate::htmltext::find_balanced_close(html, start, &RE_DIV_OPEN, &RE_DIV_CLOSE) {
        Some((close_start, _)) => (html[start..close_start].to_string(), true),
        None => (html[start..].to_string(), false),
    }
}

fn append_media_tags(body: &mut String, block: &str) {
    for caps in RE_PHOTO_BG.captures_iter(block) {
        let src = crate::htmltext::unescape_html(&caps[1]);
        body.push_str(&format!(
            "<img src=\"{}\">",
            crate::htmltext::escape_attr(&src)
        ));
    }
    for caps in RE_VIDEO_SRC.captures_iter(block) {
        let src = crate::htmltext::unescape_html(&caps[1]);
        body.push_str(&format!(
            "<video src=\"{}\"></video>",
            crate::htmltext::escape_attr(&src)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_name_valid() {
        assert_eq!(
            parse_channel_name("https://t.me/telegram"),
            Some("telegram".into())
        );
        assert_eq!(
            parse_channel_name("https://telegram.me/telegram"),
            Some("telegram".into())
        );
        assert_eq!(
            parse_channel_name("https://t.me/s/telegram"),
            Some("telegram".into())
        );
        assert_eq!(
            parse_channel_name("https://t.me/some_channel_1"),
            Some("some_channel_1".into())
        );
    }

    #[test]
    fn parse_channel_name_invalid() {
        assert_eq!(parse_channel_name("https://t.me/abcd"), None);
        assert_eq!(parse_channel_name("https://t.me/+abcde"), None);
        assert_eq!(parse_channel_name("https://t.me/share/url?url=x"), None);
        assert_eq!(parse_channel_name("https://t.me/joinchat/ABCDEF"), None);
        assert_eq!(parse_channel_name("https://t.me/c/1234567890/42"), None);
        assert_eq!(parse_channel_name("https://t.me/"), None);
        assert_eq!(parse_channel_name("https://example.com/telegram"), None);
    }

    #[test]
    fn extract_title_works() {
        let html = r#"<div class="tgme_channel_info_header_title"><span dir="auto">Telegram News</span></div>"#;
        assert_eq!(extract_channel_title(html), Some("Telegram News".into()));
        assert_eq!(extract_channel_title("<div>nothing</div>"), None);
    }

    #[test]
    fn strip_tags_for_title_works() {
        let html = r#"Hello <b>world</b> <a href="https://example.com">link</a>"#;
        assert_eq!(crate::fetcher::html_to_plain(html), "Hello world link");
        assert_eq!(crate::fetcher::html_to_plain(""), "");
    }

    #[test]
    fn extract_reply_blockquote_works() {
        let block = r#"<div class="tgme_widget_message_bubble"><a class="tgme_widget_message_reply user-color-default" href="https://t.me/some_channel/1"><div class="tgme_widget_message_author accent_color"><span class="tgme_widget_message_author_name" dir="auto">News Channel</span></div><div class="tgme_widget_message_text js-message_reply_text" dir="auto">quoted original content</div></a><div class="tgme_widget_message_text js-message_text">main text</div></div>"#;
        let q = extract_reply_blockquote(block);
        assert!(q.starts_with("<blockquote>"));
        assert!(q.ends_with("</blockquote>"));
        assert!(q.contains(r#"<a href="https://t.me/some_channel/1">News Channel</a>:"#));
        assert!(q.contains("quoted original content"));
    }

    #[test]
    fn extract_reply_blockquote_none() {
        let block = r#"<div class="tgme_widget_message_text js-message_text">message without a reply</div>"#;
        assert_eq!(extract_reply_blockquote(block), "");
    }

    #[test]
    fn extract_reply_blockquote_nested_hyperlink() {
        let block = r#"<a class="tgme_widget_message_reply" href="https://t.me/src/1"><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">before<a href="https://example.com">link text</a>after</div></a>"#;
        let q = extract_reply_blockquote(block);
        assert!(q.contains("before"), "out: {}", q);
        assert!(q.contains("after"), "out: {}", q);
        assert!(
            q.contains(r#"<a href="https://example.com">link text</a>"#),
            "out: {}",
            q
        );
    }

    #[test]
    fn extract_reply_blockquote_nested_div() {
        let block = r#"<a class="tgme_widget_message_reply" href="https://t.me/src/1"><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">head<div class="quoted-inner">middle</div>tail</div></a>"#;
        let q = extract_reply_blockquote(block);
        assert!(q.contains("head"), "out: {}", q);
        assert!(q.contains("middle"), "out: {}", q);
        assert!(q.contains("tail"), "out: {}", q);
    }

    #[test]
    fn main_text_nested_div_not_truncated() {
        let block = r#"<div class="tgme_widget_message" data-post="chan/9"><div class="tgme_widget_message_text js-message_text" dir="auto">A<div class="quote">B</div>C</div><div class="after">ignored</div></div>"#;
        let html = extract_main_text_html(block, "chan/9");
        assert_eq!(html, "A<div class=\"quote\">B</div>C");
    }

    #[test]
    fn entry_id_is_stable_msgid_not_channel_username() {
        let block = r#"<div class="tgme_widget_message" data-post="old_name/9"><div class="tgme_widget_message_text js-message_text" dir="auto">hello</div></div>"#;
        let entry = message_to_entry("old_name/9", block, "Feed", Lang::En)
            .expect("message should produce an entry");
        assert_eq!(entry.id, "9");
        assert_eq!(
            entry.links.first().map(|l| l.href.as_str()),
            Some("https://t.me/old_name/9")
        );
    }

    #[test]
    fn published_prefers_date_container_outside_reply() {
        let block = r#"<div class="tgme_widget_message" data-post="chan/200"><a class="tgme_widget_message_reply" href="https://t.me/chan/100"><div class="tgme_widget_message_date"><time datetime="2020-01-01T00:00:00+00:00">old</time></div><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">quoted</div></a><div class="tgme_widget_message_text js-message_text">new main</div><div class="tgme_widget_message_date"><time datetime="2026-10-17T08:30:00+00:00">now</time></div></div>"#;
        let dt = extract_published(block).expect("a publish time");
        let expected = DateTime::parse_from_rfc3339("2026-10-17T08:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(dt, expected);
    }

    #[test]
    fn published_falls_back_after_reply_anchor() {
        let block = r#"<div class="tgme_widget_message" data-post="chan/200"><a class="tgme_widget_message_reply" href="https://t.me/chan/100"><time datetime="2020-01-01T00:00:00+00:00">old</time><span class="tgme_widget_message_author_name">Author</span><div class="tgme_widget_message_text js-message_reply_text">quoted</div></a><div class="tgme_widget_message_text js-message_text">new main</div><time datetime="2026-10-17T08:30:00+00:00">now</time></div>"#;
        let dt = extract_published(block).expect("a publish time");
        let expected = DateTime::parse_from_rfc3339("2026-10-17T08:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(dt, expected);
    }

    #[test]
    fn published_without_reply_unchanged() {
        let block = r#"<div class="tgme_widget_message" data-post="chan/200"><div class="tgme_widget_message_date"><time datetime="2026-10-17T08:30:00+00:00">now</time></div><div class="tgme_widget_message_text js-message_text">plain</div></div>"#;
        let dt = extract_published(block).expect("a publish time");
        let expected = DateTime::parse_from_rfc3339("2026-10-17T08:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(dt, expected);
    }

    #[test]
    fn append_media_tags_works() {
        let block = r#"<a class="tgme_widget_message_photo_wrap" style="background-image:url('https://cdn1.telesco.pe/file/abc.jpg')"></a><video src="https://cdn1.telesco.pe/file/x.mp4?token=t"></video>"#;
        let mut body = String::from("text");
        append_media_tags(&mut body, block);
        assert!(body.contains("<img src=\"https://cdn1.telesco.pe/file/abc.jpg\">"));
        assert!(body.contains("<video src=\"https://cdn1.telesco.pe/file/x.mp4?token=t\"></video>"));
    }

    #[test]
    fn append_media_tags_ignores_non_photo_thumbs() {
        let block = r#"<a class="tgme_widget_message_video_thumb" style="background-image:url('https://cdn1.telesco.pe/file/poster.jpg')"></a><div class="tgme_widget_message_reply_thumb" style="background-image:url('https://cdn1.telesco.pe/file/reply.jpg')"></div>"#;
        let mut body = String::from("text");
        append_media_tags(&mut body, block);
        assert!(!body.contains("poster.jpg"));
        assert!(!body.contains("reply.jpg"));
    }

    #[test]
    fn message_block_split_works() {
        let html = r#"<div class="tgme_widget_message_wrap"><div class="tgme_widget_message" data-post="telegram/441"><div class="tgme_widget_message_text">first</div></div></div><div class="tgme_widget_message_wrap"><div class="tgme_widget_message" data-post="telegram/442"><div class="tgme_widget_message_text">second</div></div></div>"#;
        let blocks = split_message_blocks(html);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "telegram/441");
        assert_eq!(blocks[1].0, "telegram/442");
        assert!(blocks[0].1.contains("first"));
        assert!(!blocks[0].1.contains("second"));
        assert!(blocks[1].1.contains("second"));
    }

    #[test]
    fn message_block_split_skips_non_post_data_post() {
        let html = r#"<div data-post="notapost">x</div><div data-post="telegram/1">real</div>"#;
        let blocks = split_message_blocks(html);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, "telegram/1");
    }

    #[test]
    fn non_text_messages_keep_a_link_entry() {
        let poll = r#"<div class="tgme_widget_message_poll"><div class="tgme_widget_message_poll_question">Q?</div></div>"#;
        let entry = message_to_entry("chan/1", poll, "Chan", Lang::Zh).expect("poll entry");
        assert_eq!(entry.title.as_ref().unwrap().content, "(媒体消息)");
        let body = entry.content.unwrap().body.unwrap();
        assert!(body.contains("https://t.me/chan/1"));
        assert!(body.contains('🗳'));

        let en = message_to_entry("chan/1", poll, "Chan", Lang::En).expect("poll entry");
        assert_eq!(en.title.as_ref().unwrap().content, "(Media message)");

        let doc = r#"<div class="tgme_widget_message_document_wrap"><span class="tgme_widget_message_document_title">report.pdf</span></div>"#;
        let entry = message_to_entry("chan/2", doc, "Chan", Lang::Zh).expect("document entry");
        let body = entry.content.unwrap().body.unwrap();
        assert!(body.contains("https://t.me/chan/2"));
        assert!(body.contains("report.pdf"));

        let service = r#"<div class="tgme_widget_message_service">joined the channel</div>"#;
        assert!(message_to_entry("chan/3", service, "Chan", Lang::Zh).is_none());
    }
}
