//! Telegram-HTML helpers: entities, visible length, truncation and splitting.

use std::sync::LazyLock;

pub static RE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());
static RE_ANCHOR_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?i)^<a\b[^>]*?\bhref\s*=\s*["'][^"']+["']"#).unwrap());
static RE_ANCHOR_CLOSE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)^</a\s*>").unwrap());
static RE_HREF_VALUE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?i)\bhref\s*=\s*["']([^"']+)["']"#).unwrap());

pub fn is_safe_href(href: &str) -> bool {
    let lower = href.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("tg://")
        || lower.starts_with("mailto:")
}

fn anchor_href(tag: &str) -> Option<&str> {
    RE_HREF_VALUE
        .captures(tag)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
}
static RE_ENTITY_REF: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^&(?:[a-zA-Z][a-zA-Z0-9]{1,31}|#[1-9][0-9]{0,6}|#[xX][0-9a-fA-F]{1,6});")
        .unwrap()
});
static RE_BQ_OPEN_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<blockquote\b[^>]*>").unwrap());
static RE_BQ_CLOSE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</blockquote\s*>").unwrap());
static RE_PRE_CLOSE_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)</pre\s*>").unwrap());
static RE_PRE_TAG_OPEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)<pre\b[^>]*>").unwrap());

pub fn find_balanced_close(
    text: &str,
    open_end: usize,
    open_re: &regex::Regex,
    close_re: &regex::Regex,
) -> Option<(usize, usize)> {
    let mut depth = 1usize;
    let mut cursor = open_end;
    loop {
        let next_open = open_re
            .find(&text[cursor..])
            .map(|m| (cursor + m.start(), cursor + m.end()));
        let next_close = close_re
            .find(&text[cursor..])
            .map(|m| (cursor + m.start(), cursor + m.end()));
        let (span, is_open) = match (next_open, next_close) {
            (Some(o), Some(c)) => {
                if o.0 <= c.0 {
                    (o, true)
                } else {
                    (c, false)
                }
            }
            (Some(o), None) => (o, true),
            (None, Some(c)) => (c, false),
            (None, None) => return None,
        };
        if is_open {
            depth += 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return Some(span);
            }
        }
        cursor = span.1;
    }
}

pub fn visible_len(html: &str) -> usize {
    let plain = RE_TAG.replace_all(html, "");
    utf16_len(&decode_entities(&plain))
}

pub fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

pub fn take_utf16_prefix(s: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        let lu = c.len_utf16();
        if units + lu > max_units {
            if i == 0 {
                return &s[..c.len_utf8()];
            }
            return &s[..i];
        }
        units += lu;
    }
    s
}

pub fn truncate_text(s: &str, max_units: usize) -> String {
    if utf16_len(s) <= max_units {
        return s.to_string();
    }
    format!("{}…", take_utf16_prefix(s, max_units.saturating_sub(1)))
}

// Truncation is visible-length based: keep anchors clickable, close an open <a>, drop empty links, and never split an entity.
pub fn truncate_html_keep_links(html: &str, max_visible: usize) -> (String, bool) {
    let mut out = String::with_capacity(html.len().min(256));
    let mut pending_open: Option<String> = None;
    let mut in_link = false;
    let mut visible = 0usize;
    let mut truncated = false;
    let mut i = 0usize;

    while i < html.len() {
        let rest = &html[i..];
        if rest.starts_with('<') {
            if let Some(gt) = rest.find('>') {
                let tag = &rest[..gt + 1];
                if RE_ANCHOR_CLOSE.is_match(tag) {
                    if in_link {
                        out.push_str(tag);
                        in_link = false;
                    }
                    pending_open = None;
                } else if RE_ANCHOR_OPEN.is_match(tag)
                    && !in_link
                    && anchor_href(tag).is_some_and(is_safe_href)
                {
                    pending_open = Some(tag.to_string());
                } else {
                    if visible >= max_visible {
                        truncated = has_visible_content(&rest[gt + 1..]);
                        break;
                    }
                    out.push(' ');
                    visible += 1;
                }
                let after = gt + 1;
                i += after;
                if visible >= max_visible && !in_link && pending_open.is_none() {
                    truncated = has_visible_content(&rest[after..]);
                    break;
                }
                continue;
            }
        }
        let mut entity: Option<usize> = None;
        let mut ch = ' ';
        let mut ch_adv = 0usize;
        if let Some(m) = RE_ENTITY_REF.find(rest) {
            entity = Some(m.end());
        } else {
            ch = rest.chars().next().unwrap();
            ch_adv = ch.len_utf8();
        }
        let is_space = match entity {
            Some(len) => entity_is_space(&rest[..len]),
            None => ch.is_whitespace(),
        };
        if visible >= max_visible {
            if is_space {
                i += entity.unwrap_or(ch_adv);
                continue;
            }
            truncated = true;
            break;
        }
        if let Some(tag) = pending_open.take() {
            out.push_str(&tag);
            in_link = true;
        }
        match entity {
            Some(len) => {
                out.push_str(&rest[..len]);
                i += len;
            }
            None => {
                out.push(ch);
                i += ch_adv;
            }
        }
        visible += if entity.is_some() { 1 } else { ch.len_utf16() };
    }

    if in_link {
        out.push_str("</a>");
    }
    (out, truncated)
}

fn has_visible_content(s: &str) -> bool {
    let mut i = 0usize;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with('<') {
            if let Some(gt) = rest.find('>') {
                i += gt + 1;
                continue;
            }
        }
        if let Some(m) = RE_ENTITY_REF.find(rest) {
            if !entity_is_space(&rest[..m.end()]) {
                return true;
            }
            i += m.end();
            continue;
        }
        let c = rest.chars().next().unwrap();
        if !c.is_whitespace() {
            return true;
        }
        i += c.len_utf8();
    }
    false
}

fn entity_is_space(entity: &str) -> bool {
    matches!(
        entity.to_ascii_lowercase().as_str(),
        "&nbsp;" | "&#160;" | "&#xa0;"
    )
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn format_chat_id_html(chat_id: &str) -> String {
    if chat_id.starts_with('@') {
        escape_html(chat_id)
    } else {
        format!("<code>{}</code>", escape_html(chat_id))
    }
}

pub fn format_chat_title_html(title: &str, chat_id: &str) -> String {
    if title.is_empty() || title == chat_id {
        escape_html(title)
    } else {
        format!("<b>{}</b>", escape_html(title))
    }
}

pub fn decode_entities(s: &str) -> String {
    const MAX_ENTITY_BODY: usize = 32;
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let candidate = &after[1..];
        let mut window_end = candidate.len().min(MAX_ENTITY_BODY + 1);
        while !candidate.is_char_boundary(window_end) {
            window_end -= 1;
        }
        let Some(semi) = candidate[..window_end].find(';') else {
            out.push('&');
            rest = candidate;
            continue;
        };
        if semi == 0 || semi > MAX_ENTITY_BODY {
            out.push('&');
            rest = candidate;
            continue;
        }
        let body = &candidate[..semi];
        let decoded = if let Some(num) = body.strip_prefix('#') {
            let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                num.parse::<u32>().ok()
            };
            code.and_then(char::from_u32).map(|c| {
                if c == '\u{a0}' {
                    " ".to_string()
                } else if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
                    String::new()
                } else {
                    c.to_string()
                }
            })
        } else {
            crate::htmlentities::named(body).map(str::to_string)
        };
        match decoded {
            Some(value) => {
                out.push_str(&value);
                rest = &candidate[semi + 1..];
            }
            None => {
                out.push('&');
                rest = candidate;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn strip_html_tags(s: &str) -> String {
    decode_entities(&RE_TAG.replace_all(s, ""))
}

pub fn strip_html_keep_links(s: &str, max: usize) -> String {
    static RE_ANCHOR: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<a\b[^>]*\bhref\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a\s*>"#)
            .unwrap()
    });
    let plain = |html: &str| decode_entities(&RE_TAG.replace_all(html, ""));

    let mut out = String::new();
    let mut last = 0;
    for caps in RE_ANCHOR.captures_iter(s) {
        let Some(m) = caps.get(0) else {
            continue;
        };
        out.push_str(&plain(&s[last..m.start()]));
        let href = caps
            .get(1)
            .map(|m| decode_entities(m.as_str()))
            .unwrap_or_default();
        let label = plain(caps.get(2).map(|m| m.as_str()).unwrap_or(""));
        let with_href = format!("{href} {label}");
        if utf16_len(&out).saturating_add(utf16_len(&with_href)) <= max {
            out.push_str(&with_href);
        } else {
            out.push_str(&label);
        }
        last = m.end();
    }
    out.push_str(&plain(&s[last..]));
    if utf16_len(&out) > max {
        take_utf16_prefix(&out, max).to_string()
    } else {
        out
    }
}

pub fn merge_email_mention_anchors(html: &str) -> String {
    static RE_EMAIL_SPLIT_ANCHOR: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)(?P<local>[a-z0-9._%+\-]+)<a\b[^>]*>\s*(?P<at>@[a-z0-9](?:[a-z0-9\-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9\-]*[a-z0-9])?)*)\s*</a>(?P<rest>(?:\.[a-z0-9](?:[a-z0-9\-]*[a-z0-9])?)*)"#,
        )
        .unwrap()
    });
    RE_EMAIL_SPLIT_ANCHOR
        .replace_all(html, |caps: &regex::Captures| {
            let email = format!("{}{}{}", &caps["local"], &caps["at"], &caps["rest"]);
            if is_plausible_email(&email) {
                escape_html(&email)
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

fn is_plausible_email(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.len() > 64
        || email.len() > 254
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
    {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2
        || labels.iter().any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
        return false;
    }
    let tld = labels.last().copied().unwrap_or_default();
    tld.len() >= 2 && (tld.chars().all(|c| c.is_ascii_alphabetic()) || tld.starts_with("xn--"))
}

pub fn unescape_html(s: &str) -> String {
    static RE_ENTITY_NUM: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"&#(\d+);").unwrap());
    static RE_ENTITY_HEX: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)&#x([0-9a-f]+);").unwrap());
    let s = RE_ENTITY_NUM
        .replace_all(s, |caps: &regex::Captures| {
            caps[1]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string())
        })
        .to_string();
    let s = RE_ENTITY_HEX
        .replace_all(&s, |caps: &regex::Captures| {
            u32::from_str_radix(&caps[1], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string())
        })
        .to_string();
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

pub fn extract_cookie_value(set_cookie: &str, name: &str) -> Option<String> {
    let prefix = format!("{}=", name);
    set_cookie.split(';').find_map(|part| {
        part.trim()
            .strip_prefix(prefix.as_str())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
    })
}

pub fn normalize_blockquote_fold(html: &str) -> String {
    const FOLD_THRESHOLD: usize = 200;
    const HEADER_MAX_VISIBLE: usize = 50;

    let mut out = String::with_capacity(html.len());
    let mut pos = 0usize;
    while let Some(open) = RE_BQ_OPEN_TAG.find(&html[pos..]) {
        let open_start = pos + open.start();
        let inner_start = pos + open.end();
        out.push_str(&html[pos..open_start]);
        let close_span = find_balanced_close(html, inner_start, &RE_BQ_OPEN_TAG, &RE_BQ_CLOSE_TAG);
        let Some((close_start, close_end)) = close_span else {
            out.push_str(&html[open_start..]);
            pos = html.len();
            break;
        };
        let mut inner = html[inner_start..close_start].trim().to_string();
        if let Some(p) = inner.find("\n\n") {
            let first_para = &inner[..p];
            let first_visible = visible_len(first_para);
            if first_visible <= HEADER_MAX_VISIBLE {
                inner = format!("{}\n{}", first_para, &inner[p + 2..]);
            }
        }
        let visible = visible_len(&inner);
        if visible > FOLD_THRESHOLD {
            out.push_str(&format!("<blockquote expandable>{}</blockquote>", inner));
        } else {
            out.push_str(&format!("<blockquote>{}</blockquote>", inner));
        }
        pos = close_end;
    }
    out.push_str(&html[pos..]);
    out
}

pub fn split_html(text: &str, limit: usize) -> Vec<String> {
    if visible_len(text) <= limit {
        return vec![text.to_string()];
    }

    let raw_paras: Vec<&str> = text.split("\n\n").collect();
    let mut segments: Vec<String> = Vec::new();
    let mut in_block = false;
    let mut block_buf = String::new();

    for para in raw_paras {
        if para.is_empty() {
            continue;
        }
        if in_block {
            block_buf.push_str("\n\n");
            block_buf.push_str(para);
            if RE_BQ_CLOSE_TAG.is_match(para) || RE_PRE_CLOSE_TAG.is_match(para) {
                in_block = false;
                segments.push(block_buf.clone());
                block_buf.clear();
            }
        } else if (RE_BQ_OPEN_TAG.is_match(para) && !RE_BQ_CLOSE_TAG.is_match(para))
            || (RE_PRE_TAG_OPEN.is_match(para) && !RE_PRE_CLOSE_TAG.is_match(para))
        {
            in_block = true;
            block_buf = para.to_string();
        } else {
            segments.push(para.to_string());
        }
    }
    if !block_buf.is_empty() {
        segments.push(block_buf);
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for para in segments {
        let candidate = if current.is_empty() {
            para.clone()
        } else {
            format!("{}\n\n{}", current, para)
        };

        if visible_len(&candidate) <= limit {
            current = candidate;
        } else if visible_len(&para) > limit {
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }
            if RE_PRE_TAG_OPEN.is_match(&para) {
                static RE_PRE_OPEN: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| {
                        regex::Regex::new(r"(?is)<pre\b[^>]*><code\b[^>]*>").unwrap()
                    });
                static RE_PRE_BARE: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<pre\b[^>]*>").unwrap());
                let code_match = RE_PRE_OPEN.find(para.as_str());
                let bare_match = RE_PRE_BARE.find(para.as_str());
                let open_match = code_match.as_ref().or(bare_match.as_ref());
                let (open_tag, close_tag) = if let Some(m) = code_match.as_ref() {
                    (m.as_str().to_string(), "</code></pre>".to_string())
                } else if let Some(m) = bare_match.as_ref() {
                    (m.as_str().to_string(), "</pre>".to_string())
                } else {
                    ("<pre><code>".to_string(), "</code></pre>".to_string())
                };
                let pre_start = open_match.map(|m| m.start()).unwrap_or(0);
                let pre_open_end = open_match.map(|m| m.end()).unwrap_or(pre_start);
                let close_span = RE_PRE_CLOSE_TAG
                    .find(&para[pre_open_end..])
                    .map(|m| (pre_open_end + m.start(), pre_open_end + m.end()));
                let (block_end, suffix) = match close_span {
                    Some((_, end)) => (end, &para[end..]),
                    None => (para.len(), ""),
                };
                let prefix = para[..pre_start].trim_end_matches('\n').to_string();
                let block = &para[pre_start..block_end];
                let inner = {
                    let stripped = if code_match.is_some() {
                        RE_PRE_OPEN.replace_all(block, "").to_string()
                    } else {
                        RE_PRE_BARE.replace_all(block, "").to_string()
                    };
                    stripped.replace(&close_tag, "")
                };
                let inner_limit =
                    limit.saturating_sub(utf16_len(&open_tag) + utf16_len(&close_tag));
                // An oversized single line must be hard-split; a >4096 chunk fails every retry and is abandoned after 72h.
                let mut code_chunks: Vec<String> = Vec::new();
                let mut cur = String::new();
                for line in inner.split('\n') {
                    let mut rest = line;
                    while utf16_len(rest) > inner_limit {
                        let head = take_utf16_prefix(rest, inner_limit);
                        let candidate = if cur.is_empty() {
                            head.to_string()
                        } else {
                            format!("{}\n{}", cur, head)
                        };
                        if utf16_len(&candidate) <= inner_limit {
                            cur = candidate;
                        } else {
                            code_chunks.push(std::mem::take(&mut cur));
                            cur = head.to_string();
                        }
                        rest = &rest[head.len()..];
                    }
                    if !rest.is_empty() {
                        let candidate = if cur.is_empty() {
                            rest.to_string()
                        } else {
                            format!("{}\n{}", cur, rest)
                        };
                        if utf16_len(&candidate) <= inner_limit {
                            cur = candidate;
                        } else {
                            code_chunks.push(std::mem::take(&mut cur));
                            cur = rest.to_string();
                        }
                    }
                }
                if !cur.is_empty() {
                    code_chunks.push(cur);
                }
                if !prefix.is_empty() {
                    if visible_len(&prefix) <= limit {
                        chunks.push(prefix);
                    } else {
                        for sub in split_html_safe(&prefix, limit) {
                            chunks.push(sub);
                        }
                    }
                }
                for c in &code_chunks {
                    chunks.push(format!("{}{}{}", open_tag, c, close_tag));
                }
                if !suffix.trim().is_empty() {
                    chunks.extend(split_html(suffix, limit));
                }
            } else if RE_BQ_OPEN_TAG.is_match(&para) {
                const BQ_OPEN: &str = "<blockquote expandable>";
                const BQ_CLOSE: &str = "</blockquote>";

                let open_match = RE_BQ_OPEN_TAG.find(para.as_str());
                let bq_start = open_match.map(|m| m.start()).unwrap_or(0);
                let open_end = open_match.map(|m| m.end()).unwrap_or(bq_start);
                let close_span =
                    find_balanced_close(&para, open_end, &RE_BQ_OPEN_TAG, &RE_BQ_CLOSE_TAG);
                let (inner_end, suffix) = match close_span {
                    Some((close_start, close_end)) => (close_start, &para[close_end..]),
                    None => (para.len(), ""),
                };
                let prefix = para[..bq_start].trim_end_matches('\n').to_string();
                let inner = &para[open_end..inner_end];
                let inner_limit =
                    limit.saturating_sub(BQ_OPEN.chars().count() + BQ_CLOSE.chars().count());

                let prefix_visible = visible_len(&prefix);
                let budget = if prefix.is_empty() {
                    inner_limit
                } else {
                    inner_limit.saturating_sub(prefix_visible + 1)
                };
                let merge_first = !prefix.is_empty() && prefix_visible <= limit && budget >= 100;

                let subs = if merge_first {
                    split_html(inner, budget)
                } else {
                    split_html(inner, inner_limit)
                };

                if prefix.is_empty() {
                    for sub in &subs {
                        chunks.push(format!("{}{}{}", BQ_OPEN, sub, BQ_CLOSE));
                    }
                } else if merge_first {
                    let first_wrapped = format!(
                        "{}{}{}",
                        BQ_OPEN,
                        subs.first().map(String::as_str).unwrap_or(""),
                        BQ_CLOSE
                    );
                    chunks.push(format!("{}\n{}", prefix, first_wrapped));
                    for sub in subs.iter().skip(1) {
                        chunks.push(format!("{}{}{}", BQ_OPEN, sub, BQ_CLOSE));
                    }
                } else {
                    if prefix_visible <= limit {
                        chunks.push(prefix.clone());
                    } else {
                        for sub in split_html_safe(&prefix, limit) {
                            chunks.push(sub);
                        }
                    }
                    for sub in &subs {
                        chunks.push(format!("{}{}{}", BQ_OPEN, sub, BQ_CLOSE));
                    }
                }
                if !suffix.trim().is_empty() {
                    chunks.extend(split_html(suffix, limit));
                }
            } else {
                if !current.is_empty() {
                    chunks.push(current.clone());
                    current.clear();
                }
                for sub in split_html_safe(&para, limit) {
                    chunks.push(sub);
                }
            }
        } else {
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }
            current = para;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    rebalance_chunks(&mut chunks);
    for chunk in &mut chunks {
        *chunk = balance_html_tags(chunk);
    }
    chunks.retain(|c| visible_len(c) > 0);
    chunks
}

pub fn balance_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < html.len() {
        let rest = &html[i..];
        if rest.starts_with('<') {
            if let Some(gt) = rest.find('>') {
                let tag = &rest[..gt + 1];
                if let Some(name) = tag_name(tag) {
                    if tag.starts_with("</") {
                        if stack.last().is_some_and(|open| open == &name) {
                            stack.pop();
                            out.push_str(tag);
                        }
                    } else {
                        out.push_str(tag);
                        if !VOID_TAGS.contains(&name.as_str()) && !tag.ends_with("/>") {
                            stack.push(name);
                        }
                    }
                } else {
                    out.push_str(tag);
                }
                i += gt + 1;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    for name in stack.iter().rev() {
        out.push_str("</");
        out.push_str(name);
        out.push('>');
    }
    out
}

// Every split chunk must be well-formed Telegram HTML: close open tags in the head and re-open them in the tail.
fn rebalance_chunks(chunks: &mut [String]) {
    let mut carried: Vec<String> = Vec::new();
    for i in 0..chunks.len() {
        if !carried.is_empty() {
            let mut s = String::new();
            for tag in carried.drain(..) {
                s.push_str(&tag);
            }
            s.push_str(&chunks[i]);
            chunks[i] = s;
        }
        if i + 1 == chunks.len() {
            break;
        }
        let scan = scan_split_candidates(&chunks[i], usize::MAX);
        if scan.end_stack.is_empty() {
            continue;
        }
        let mut closers = String::new();
        for (name, _) in scan.end_stack.iter().rev() {
            closers.push_str(&format!("</{}>", name));
        }
        chunks[i].push_str(&closers);
        carried = scan.end_stack.into_iter().map(|(_, tag)| tag).collect();
    }
}

const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

struct CutCandidate {
    pos: usize,
    openers: String,
    closers: String,
}

impl CutCandidate {
    fn at(pos: usize, stack: &[(String, String)]) -> Self {
        Self {
            pos,
            openers: stack.iter().map(|(_, tag)| tag.as_str()).collect(),
            closers: stack
                .iter()
                .rev()
                .map(|(name, _)| format!("</{name}>"))
                .collect(),
        }
    }
}

fn tag_name(tag: &str) -> Option<String> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    let inner = inner.strip_prefix('/').unwrap_or(inner);
    let name: String = inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!name.is_empty()).then(|| name.to_lowercase())
}

struct SplitScan {
    best: [Option<CutCandidate>; 4],
    end_stack: Vec<(String, String)>,
}

// Never split inside a tag or entity reference; a fragmented &amp; or <a href makes Telegram reject the message.
fn scan_split_candidates(text: &str, limit: usize) -> SplitScan {
    let window = take_utf16_prefix(text, limit);
    let mut best: [Option<CutCandidate>; 4] = Default::default();
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut i = 0usize;
    while i < window.len() {
        let rest = &window[i..];
        if rest.starts_with('<') {
            let Some(gt) = rest.find('>') else {
                for slot in best.iter_mut() {
                    if slot.as_ref().is_some_and(|c| c.pos > i) {
                        *slot = None;
                    }
                }
                return SplitScan {
                    best,
                    end_stack: stack,
                };
            };
            let tag = &rest[..gt + 1];
            let end = i + gt + 1;
            match tag_name(tag) {
                None => best[3] = Some(CutCandidate::at(end, &stack)),
                Some(name) if tag.starts_with("</") => {
                    if let Some(idx) = stack.iter().rposition(|(n, _)| *n == name) {
                        stack.truncate(idx);
                    }
                    best[0] = Some(CutCandidate::at(end, &stack));
                }
                Some(name) if tag.ends_with("/>") || VOID_TAGS.contains(&name.as_str()) => {
                    if i > 0 {
                        best[1] = Some(CutCandidate::at(i, &stack));
                    }
                    best[3] = Some(CutCandidate::at(end, &stack));
                }
                Some(name) => {
                    if i > 0 {
                        best[1] = Some(CutCandidate::at(i, &stack));
                    }
                    stack.push((name, tag.to_string()));
                }
            }
            i = end;
            continue;
        }
        if let Some(m) = RE_ENTITY_REF.find(rest) {
            i += m.end();
            best[3] = Some(CutCandidate::at(i, &stack));
            continue;
        }
        let c = rest.chars().next().unwrap();
        let next = i + c.len_utf8();
        if c.is_whitespace() {
            best[2] = Some(CutCandidate::at(next, &stack));
        }
        i = next;
    }
    SplitScan {
        best,
        end_stack: stack,
    }
}

fn scan_raw_cut(text: &str, max_units: usize) -> (usize, Vec<(String, String)>) {
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut units = 0usize;
    let mut i = 0usize;
    let mut best = 0usize;
    let mut best_stack: Vec<(String, String)> = Vec::new();

    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with('<') {
            let Some(gt) = rest.find('>') else {
                break;
            };
            let tag = &rest[..gt + 1];
            let tag_units = utf16_len(tag);
            if units + tag_units > max_units {
                break;
            }
            let is_close = tag.starts_with("</");
            let name = tag_name(tag);
            let self_closing = tag.ends_with("/>");
            if let Some(name) = name {
                if is_close {
                    if let Some(idx) = stack.iter().rposition(|(n, _)| *n == name) {
                        stack.truncate(idx);
                    }
                } else if !self_closing && !VOID_TAGS.contains(&name.as_str()) {
                    if i > 0 && units > 0 {
                        best = i;
                        best_stack = stack.clone();
                    }
                    stack.push((name, tag.to_string()));
                    units += tag_units;
                    i += tag.len();
                    continue;
                }
            }
            units += tag_units;
            i += tag.len();
            best = i;
            best_stack = stack.clone();
            continue;
        }
        if let Some(m) = RE_ENTITY_REF.find(rest) {
            let entity_units = utf16_len(&rest[..m.end()]);
            if units + entity_units > max_units {
                break;
            }
            units += entity_units;
            i += m.end();
            best = i;
            best_stack = stack.clone();
            continue;
        }
        let c = rest.chars().next().unwrap();
        let char_units = c.len_utf16();
        if units + char_units > max_units {
            break;
        }
        units += char_units;
        i += c.len_utf8();
        best = i;
        best_stack = stack.clone();
    }

    (best, best_stack)
}

fn split_plain_utf16(text: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while utf16_len(rest) > limit {
        let cut = take_utf16_prefix(rest, limit);
        if cut.is_empty() {
            break;
        }
        out.push(cut.to_string());
        rest = &rest[cut.len()..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

pub fn split_html_safe(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = text.to_string();

    let mut iters = 0usize;
    while utf16_len(&remaining) > limit {
        iters += 1;
        if iters > 200_000 {
            tracing::warn!(
                limit = limit,
                remaining_units = utf16_len(&remaining),
                "split_html_safe hit the iteration guard; emitting an over-long remainder"
            );
            chunks.push(remaining);
            return chunks;
        }
        let scan = scan_split_candidates(&remaining, limit);
        let usable = |c: &&CutCandidate| {
            utf16_len(&remaining[..c.pos]) + utf16_len(&c.closers) <= limit
                && utf16_len(&remaining[..c.pos]) > utf16_len(&c.openers)
        };
        if let Some(c) = scan.best.iter().flatten().find(usable) {
            chunks.push(format!("{}{}", &remaining[..c.pos], c.closers));
            remaining = format!("{}{}", c.openers, &remaining[c.pos..]);
            continue;
        }
        let mut budget = limit;
        let (mut cut, mut cut_stack) = scan_raw_cut(&remaining, budget);
        for _ in 0..8 {
            if cut == 0 {
                break;
            }
            let closers_len: usize = cut_stack
                .iter()
                .rev()
                .map(|(n, _)| utf16_len(&format!("</{n}>")))
                .sum();
            if utf16_len(&remaining[..cut]) + closers_len <= limit {
                break;
            }
            let new_budget = limit.saturating_sub(closers_len).max(1);
            if new_budget >= budget {
                break;
            }
            budget = new_budget;
            let next = scan_raw_cut(&remaining, budget);
            cut = next.0;
            cut_stack = next.1;
        }
        if cut == 0 {
            tracing::warn!(
                limit = limit,
                remaining_units = utf16_len(&remaining),
                "split_html_safe found no safe raw cut; falling back to plain text"
            );
            let plain = strip_html_tags(&remaining);
            if visible_len(&plain) > limit {
                chunks.extend(split_plain_utf16(&plain, limit));
            } else {
                chunks.push(remaining);
            }
            return chunks;
        }
        let openers: String = cut_stack.iter().map(|(_, t)| t.as_str()).collect();
        let closers: String = cut_stack
            .iter()
            .rev()
            .map(|(n, _)| format!("</{n}>"))
            .collect();
        let head_len = utf16_len(&remaining[..cut]);
        if head_len + utf16_len(&closers) > limit || head_len <= utf16_len(&openers) {
            tracing::warn!(
                limit = limit,
                head_units = head_len,
                closers_units = utf16_len(&closers),
                openers_units = utf16_len(&openers),
                "split_html_safe cannot make progress within the limit; falling back to plain text"
            );
            let plain = strip_html_tags(&remaining);
            if visible_len(&plain) > limit {
                chunks.extend(split_plain_utf16(&plain, limit));
            } else {
                chunks.push(remaining);
            }
            return chunks;
        }
        chunks.push(format!("{}{}", &remaining[..cut], closers));
        remaining = format!("{}{}", openers, &remaining[cut..]);
    }

    if !remaining.is_empty() {
        chunks.push(remaining);
    }
    chunks
}

pub fn merge_adjacent_blockquotes(text: &str) -> String {
    const BQ_OPEN: &str = "<blockquote expandable>";
    const BQ_CLOSE: &str = "</blockquote>";
    let seam = format!("{}\n\n{}", BQ_CLOSE, BQ_OPEN);
    text.replace(&seam, "\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_html_tags_closes_unclosed_and_drops_orphans() {
        assert_eq!(
            balance_html_tags("<blockquote>hello"),
            "<blockquote>hello</blockquote>"
        );
        assert_eq!(balance_html_tags("hello</blockquote>"), "hello");
        assert_eq!(
            balance_html_tags("<a href=\"https://x\">text</a>"),
            "<a href=\"https://x\">text</a>"
        );
    }

    #[test]
    fn plain_fallback_keeps_anchor_destinations() {
        assert_eq!(
            strip_html_keep_links(
                r#"before <a href="https://example.com/a?x=1&amp;y=2">link</a> after"#,
                4096
            ),
            "before https://example.com/a?x=1&y=2 link after"
        );
    }

    #[test]
    fn plain_fallback_drops_only_urls_that_exceed_budget() {
        let long_url = format!("https://example.com/{}", "x".repeat(80));
        let html = format!(
            r#"<a href="{long_url}">first</a> <a href="https://example.com/short">second</a>"#
        );
        let plain = strip_html_keep_links(&html, 60);
        assert!(utf16_len(&plain) <= 60);
        assert!(!plain.contains(&long_url));
        assert!(plain.contains("https://example.com/short"));
        assert!(plain.contains("first"));
        assert!(plain.contains("second"));
    }

    #[test]
    fn safe_href_allows_only_telegram_renderable_schemes() {
        assert!(is_safe_href("https://example.com/a"));
        assert!(is_safe_href("http://example.com/a"));
        assert!(is_safe_href("tg://resolve?domain=x"));
        assert!(is_safe_href("mailto:a@example.com"));
        assert!(!is_safe_href("javascript:alert(1)"));
        assert!(!is_safe_href("data:text/html,x"));
        assert!(!is_safe_href("/relative"));
    }

    #[test]
    fn merge_email_mention_anchors_restores_split_email() {
        let html = r#"发送至：user<a href='https://m.weibo.cn/n/example'>@example</a>.com。"#;
        assert_eq!(
            merge_email_mention_anchors(html),
            "发送至：user@example.com。"
        );
    }

    #[test]
    fn merge_email_mention_anchors_keeps_legit_mentions() {
        let html = r#"关注<a href="https://m.weibo.cn/n/example">@example</a>获取更新"#;
        assert_eq!(merge_email_mention_anchors(html), html);
        let spaced = r#"联系 <a href="https://m.weibo.cn/n/example">@example</a>"#;
        assert_eq!(merge_email_mention_anchors(spaced), spaced);
    }

    #[test]
    fn merge_email_mention_anchors_requires_valid_email() {
        let no_dot = r#"user<a href="https://m.weibo.cn/n/example">@example</a>后续"#;
        assert_eq!(merge_email_mention_anchors(no_dot), no_dot);
        let bad_tld = r#"user<a href="https://m.weibo.cn/example">@example</a>.1"#;
        assert_eq!(merge_email_mention_anchors(bad_tld), bad_tld);
    }

    #[test]
    fn merge_email_mention_anchors_handles_full_domain_and_multiple() {
        let html = r#"a.b+tag<a href="https://weibo.com/n/x">@example.com</a>
c<a href="https://weibo.com/n/y">@example.org</a>"#;
        let out = merge_email_mention_anchors(html);
        assert!(out.contains("a.b+tag@example.com"), "out: {out}");
        assert!(out.contains("c@example.org"), "out: {out}");
        assert!(!out.contains("<a "), "out: {out}");
    }

    #[test]
    fn utf16_len_ascii_and_cjk() {
        assert_eq!(utf16_len("hello"), 5);
        assert_eq!(utf16_len("中文"), 2);
    }

    #[test]
    fn utf16_len_emoji_counts_two_units() {
        assert_eq!(utf16_len("😎"), 2);
        assert_eq!(utf16_len("a😎b"), 4);
    }

    #[test]
    fn take_utf16_prefix_never_splits_char() {
        assert_eq!(take_utf16_prefix("😎abc", 1), "😎");
        assert_eq!(take_utf16_prefix("😎abc", 3), "😎a");
        assert_eq!(take_utf16_prefix("abc", 10), "abc");
    }

    #[test]
    fn decodes_html5_named_entities() {
        assert_eq!(
            decode_entities("A &mdash; B &hellip; C &rsquo;"),
            "A — B … C ’"
        );
        assert_eq!(decode_entities("x&nbsp;y &copy;"), "x y ©");
        assert_eq!(
            decode_entities("&check; &NotEqualTilde;"),
            "✓ \u{2242}\u{338}"
        );
        assert_eq!(decode_entities("a&NewLine;b"), "a\nb");
        assert_eq!(
            decode_entities("&amp;lt;"),
            "&lt;",
            "entity must decode exactly once"
        );
    }

    #[test]
    fn decodes_numeric_entities_and_keeps_whitespace_controls() {
        assert_eq!(decode_entities("a&#10;b&#9;c"), "a\nb\tc");
        assert_eq!(decode_entities("a&#x4E2D;b"), "a中b");
        assert_eq!(
            decode_entities("a&#160;b"),
            "a b",
            "numeric nbsp becomes a plain space"
        );
        assert_eq!(decode_entities("a&#0;b&#7;c"), "abc");
        assert_eq!(
            decode_entities("&unknown; &#x110000;"),
            "&unknown; &#x110000;"
        );
    }

    #[test]
    fn decode_entities_bounds_terminator_scan() {
        let input = format!("{};", "&a".repeat(50_000));
        let out = decode_entities(&input);
        assert_eq!(out, input, "unknown references must stay literal");
    }

    #[test]
    fn visible_len_ignores_tags() {
        assert_eq!(visible_len("<b>hello</b>"), 5);
        assert_eq!(visible_len(r#"<a href="https://example.com">链接</a>"#), 2);
        assert_eq!(visible_len("😎"), 2);
    }

    #[test]
    fn visible_len_decodes_entities_like_telegram() {
        assert_eq!(visible_len("a &amp; b"), 5);
        assert_eq!(visible_len("&amp;"), 1);
        assert_eq!(visible_len("&#39;"), 1);
        assert_eq!(visible_len("&nbsp;"), 1);
        assert_eq!(visible_len("&amp;lt;"), 4);
    }

    #[test]
    fn escape_html_escapes_special_chars() {
        assert_eq!(escape_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn split_html_short_text_single_chunk() {
        let out = split_html("short text", 100);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "short text");
    }

    #[test]
    fn split_html_respects_limit_per_chunk() {
        let text = ["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc", "dddddddddd"].join("\n\n");
        let out = split_html(&text, 25);
        assert!(
            out.len() >= 2,
            "expected multiple chunks, got {}",
            out.len()
        );
        for c in &out {
            assert!(visible_len(c) <= 25, "chunk exceeds limit: {:?}", c);
        }
    }

    #[test]
    fn split_html_preserves_all_visible_content() {
        let paras = ["第一段内容", "第二段内容", "第三段内容"];
        let text = paras.join("\n\n");
        let out = split_html(&text, 8);
        for p in &paras {
            assert!(
                out.iter().any(|c| c.contains(p)),
                "missing paragraph: {}",
                p
            );
        }
    }

    #[test]
    fn split_html_keeps_blockquote_atomic() {
        let bq = "<blockquote>引用第一行\n\n引用第二行</blockquote>";
        let text = format!("前置段落\n\n{}\n\n后置段落", bq);
        let out = split_html(&text, 100);
        let holders: Vec<_> = out.iter().filter(|c| c.contains("<blockquote")).collect();
        assert_eq!(
            holders.len(),
            1,
            "blockquote spread across chunks: {:?}",
            out
        );
        assert!(holders[0].contains("引用第一行"), "chunk: {:?}", holders[0]);
        assert!(holders[0].contains("引用第二行"), "chunk: {:?}", holders[0]);
        assert!(
            holders[0].contains("</blockquote>"),
            "chunk: {:?}",
            holders[0]
        );
    }

    #[test]
    fn split_html_oversized_blockquote_split_and_rewrapped() {
        let para1 = "A".repeat(40);
        let para2 = "B".repeat(40);
        let bq = format!("<blockquote>{}\n\n{}</blockquote>", para1, para2);
        let out = split_html(&bq, 78);
        assert!(
            out.len() >= 2,
            "expected >=2 chunks, got {}: {:?}",
            out.len(),
            out
        );
        for c in &out {
            let has_open = c.contains("<blockquote");
            let has_close = c.contains("</blockquote>");
            assert_eq!(has_open, has_close, "half blockquote in chunk: {:?}", c);
        }
        let joined = out.join("\u{FFFC}");
        assert!(joined.contains(&para1), "out: {:?}", out);
        assert!(joined.contains(&para2), "out: {:?}", out);
    }

    #[test]
    fn split_html_keeps_pre_block_atomic() {
        let pre = "<pre><code>line1\n\nline2</code></pre>";
        let text = format!("前置段落\n\n{}\n\n后置段落", pre);
        let out = split_html(&text, 100);
        let holders: Vec<_> = out.iter().filter(|c| c.contains("<pre>")).collect();
        assert_eq!(
            holders.len(),
            1,
            "pre block spread across chunks: {:?}",
            out
        );
        assert!(holders[0].contains("line1"), "chunk: {:?}", holders[0]);
        assert!(holders[0].contains("line2"), "chunk: {:?}", holders[0]);
        assert!(holders[0].contains("</pre>"), "chunk: {:?}", holders[0]);
    }

    #[test]
    fn split_html_oversized_pre_block_split_and_rewrapped() {
        let line1 = "A".repeat(20);
        let line2 = "B".repeat(20);
        let line3 = "C".repeat(20);
        let pre = format!(
            "<pre><code>{}\n\n{}\n\n{}</code></pre>",
            line1, line2, line3
        );
        let out = split_html(&pre, 50);
        assert!(
            out.len() >= 2,
            "expected >=2 chunks, got {}: {:?}",
            out.len(),
            out
        );
        for c in &out {
            let has_open = c.contains("<pre>");
            let has_close = c.contains("</pre>");
            assert_eq!(has_open, has_close, "half pre block in chunk: {:?}", c);
        }
        let joined = out.join("\u{FFFC}");
        assert!(joined.contains(&line1), "out: {:?}", out);
        assert!(joined.contains(&line2), "out: {:?}", out);
        assert!(joined.contains(&line3), "out: {:?}", out);
    }

    #[test]
    fn split_html_keeps_text_after_oversized_blockquote_outside() {
        let quote = "引".repeat(200);
        let text = format!("<blockquote>{quote}</blockquote>AFTER");
        let out = split_html(&text, 80);
        let tail = out.last().expect("at least one chunk");
        assert!(tail.contains("AFTER"), "suffix lost: {:?}", out);
        assert!(
            !tail.contains("<blockquote"),
            "suffix wrapped into quote: {tail:?}"
        );
        assert!(
            out.iter().any(|c| c.contains("引")),
            "quote content lost: {:?}",
            out
        );
    }

    #[test]
    fn split_html_keeps_text_after_oversized_pre_outside() {
        let code = "line\n".repeat(80);
        let text = format!("<pre>{code}</pre>AFTER");
        let out = split_html(&text, 60);
        let tail = out.last().expect("at least one chunk");
        assert!(tail.contains("AFTER"), "suffix lost: {:?}", out);
        assert!(!tail.contains("<pre"), "suffix wrapped into pre: {tail:?}");
        assert!(
            out.iter().any(|c| c.contains("line")),
            "code content lost: {:?}",
            out
        );
    }

    #[test]
    fn split_html_long_single_paragraph_hard_split() {
        let text = "字".repeat(100);
        let out = split_html(&text, 30);
        assert!(out.len() >= 4);
        for c in &out {
            assert!(visible_len(c) <= 30, "chunk exceeds limit");
        }
    }

    #[test]
    fn split_html_emoji_limit_accounting() {
        let text = ["😎😎😎😎😎"; 4].join("\n\n");
        let out = split_html(&text, 10);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn split_pre_oversized_single_line_hard_split() {
        let long_line = "x".repeat(9000);
        let html = format!("<pre><code>{}</code></pre>", long_line);
        let chunks = split_html(&html, 4096);
        assert!(
            chunks.len() > 1,
            "oversized single line must be split, got {} chunk(s)",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                utf16_len(c) <= 4096,
                "chunk exceeds limit: len={}",
                utf16_len(c)
            );
            assert!(
                c.contains("<pre"),
                "chunk missing pre open: {}",
                &c[..c.len().min(60)]
            );
            assert!(c.contains("</pre>"), "chunk missing pre close");
        }
    }

    #[test]
    fn fold_short_blockquote_stays_plain() {
        let out = normalize_blockquote_fold("<blockquote>短引用</blockquote>");
        assert_eq!(out, "<blockquote>短引用</blockquote>");
    }

    #[test]
    fn fold_long_blockquote_becomes_expandable() {
        let inner = "长".repeat(201);
        let out = normalize_blockquote_fold(&format!("<blockquote>{}</blockquote>", inner));
        assert!(out.starts_with("<blockquote expandable>"), "got: {}", out);
    }

    #[test]
    fn fold_compresses_short_header_paragraph_gap() {
        let out = normalize_blockquote_fold("<blockquote>竹新社:\n\n正文内容</blockquote>");
        assert_eq!(out, "<blockquote>竹新社:\n正文内容</blockquote>");
    }

    #[test]
    fn fold_keeps_gap_for_long_header_paragraph() {
        let header = "头".repeat(51);
        let input = format!("<blockquote>{}\n\n正文</blockquote>", header);
        let out = normalize_blockquote_fold(&input);
        assert!(
            out.contains("\n\n"),
            "long header should keep paragraph gap: {}",
            out
        );
    }

    #[test]
    fn fold_trims_inner_whitespace() {
        let out = normalize_blockquote_fold("<blockquote>  引用  </blockquote>");
        assert_eq!(out, "<blockquote>引用</blockquote>");
    }

    #[test]
    fn fold_preserves_expandable_input_tag() {
        let out = normalize_blockquote_fold("<blockquote expandable>短</blockquote>");
        assert_eq!(out, "<blockquote>短</blockquote>");
    }

    #[test]
    fn fold_nested_blockquote_no_orphan_close_tag() {
        let out = normalize_blockquote_fold(
            "<blockquote>外层 <blockquote>内层</blockquote> 结尾</blockquote>",
        );
        let opens = out.matches("<blockquote").count();
        let closes = out.matches("</blockquote>").count();
        assert_eq!(opens, closes, "orphan close tag: {}", out);
        assert!(out.contains("内层"), "inner content lost: {}", out);
        assert!(out.contains("结尾"), "trailing content lost: {}", out);
    }

    #[test]
    fn fold_adjacent_blockquotes_both_processed() {
        let out =
            normalize_blockquote_fold("<blockquote>甲</blockquote><blockquote>乙</blockquote>");
        assert_eq!(out.matches("</blockquote>").count(), 2, "got: {}", out);
        assert!(out.contains("甲") && out.contains("乙"), "got: {}", out);
    }

    fn tag_balanced(c: &str) -> bool {
        c.matches("<a ").count() == c.matches("</a>").count()
            && c.matches("<b>").count() == c.matches("</b>").count()
            && c.matches("<i>").count() == c.matches("</i>").count()
            && c.matches("<pre").count() == c.matches("</pre>").count()
    }

    #[test]
    fn split_safe_long_anchor_never_splits_tag() {
        let inner = "字".repeat(100);
        let html = format!(r#"<a href="https://ex.com/a/b?x=1&y=2">{inner}</a>"#);
        let out = split_html_safe(&html, 60);
        assert!(out.len() > 2);
        for c in &out {
            assert!(tag_balanced(c), "unbalanced anchor chunk: {c:?}");
        }
        assert_eq!(
            out.iter().map(|c| c.matches('字').count()).sum::<usize>(),
            100
        );
    }

    #[test]
    fn split_safe_raw_cut_uses_stack_at_the_cut() {
        for (limit, open, close, n) in [
            (4096usize, "<b><a href=\"u\">", "</a></b>", 8150usize),
            (1024, "<b><i>", "</i></b>", 2024),
            (1024, "<b><a href=\"u\">", "</a></b>", 2024),
        ] {
            let text = format!("{}{}{}", open, "中".repeat(n), close);
            let chunks = split_html_safe(&text, limit);
            assert!(chunks.len() >= 2, "expected a split at limit {limit}");
            for c in &chunks {
                assert!(tag_balanced(c), "unbalanced chunk at limit {limit}: {c:?}");
                assert!(
                    visible_len(c) > 0,
                    "empty visible chunk at limit {limit}: {c:?}"
                );
                assert!(
                    visible_len(c) <= limit,
                    "chunk exceeds limit {limit}: visible={} {c:?}",
                    visible_len(c)
                );
            }
            assert_eq!(
                chunks
                    .iter()
                    .map(|c| c.matches('中').count())
                    .sum::<usize>(),
                n,
                "content lost at limit {limit}"
            );
        }
    }

    #[test]
    fn split_safe_never_splits_entity() {
        let text = "ab&amp;".repeat(60);
        let out = split_html_safe(&text, 23);
        assert!(out.len() >= 5);
        for c in &out {
            assert!(!c.ends_with('&'), "chunk ends with dangling & {c}");
            for probe in ["&am", "&amp", "&#3", "&not", "&gt", "&lt"] {
                assert!(
                    !c.ends_with(probe) || c.ends_with(&format!("{probe};")),
                    "split entity {probe:?} in {c}"
                );
            }
        }
    }

    #[test]
    fn split_safe_balances_open_tags() {
        let text = format!("<b>{}</b>", "x".repeat(90));
        let out = split_html_safe(&text, 20);
        for c in &out {
            assert!(
                c.matches("<b>").count() == c.matches("</b>").count(),
                "unbalanced <b>: {c}"
            );
        }
        assert!(out.concat().matches('x').count() == 90, "content lost");
    }

    #[test]
    fn split_html_oversized_bare_pre_rewraps_wellformed() {
        let text = format!("<pre>{}</pre>", "line of code here\n".repeat(30));
        let out = split_html(&text, 50);
        assert!(out.len() >= 2, "expected multiple chunks");
        for c in &out {
            assert_eq!(
                c.matches("<pre").count(),
                c.matches("</pre>").count(),
                "unbalanced pre: {c}"
            );
            assert!(
                !c.contains("<pre><code><pre"),
                "double pre open in chunk: {c}"
            );
        }
    }

    #[test]
    fn split_html_oversized_anchor_wellformed() {
        let inner = "字".repeat(200);
        let html = format!(r#"<a href="https://ex.com/x?y=1">{inner}</a>"#);
        let out = split_html(&html, 60);
        assert!(out.len() >= 3);
        for c in &out {
            assert_eq!(
                c.matches("<a ").count(),
                c.matches("</a>").count(),
                "unbalanced anchor chunk: {c}"
            );
        }
    }

    #[test]
    fn split_html_balances_anchor_spanning_blank_lines() {
        let html = r#"<a href="https://ex.com/x">first paragraph

second paragraph</a> tail"#;
        let out = split_html(html, 25);
        assert!(out.len() >= 2);
        for c in &out {
            let scan = scan_split_candidates(c, usize::MAX);
            assert!(scan.end_stack.is_empty(), "unclosed tags in chunk: {c}");
            assert!(visible_len(c) <= 25, "chunk exceeds limit: {c}");
            assert_eq!(
                c.matches("<a ").count(),
                c.matches("</a>").count(),
                "unbalanced anchor: {c}"
            );
        }
    }

    #[test]
    fn split_html_balances_nested_blockquote() {
        let html =
            "<blockquote>outer <blockquote>inner</blockquote> text\n\nmore</blockquote> tail";
        let out = split_html(html, 20);
        assert!(out.len() >= 2);
        for c in &out {
            let scan = scan_split_candidates(c, usize::MAX);
            assert!(scan.end_stack.is_empty(), "unclosed tags in chunk: {c}");
            assert!(visible_len(c) <= 20, "chunk exceeds limit: {c}");
        }
    }

    #[test]
    fn split_html_drops_zero_visible_blockquote_fragment() {
        let html = format!("<blockquote></blockquote>\n\n{}", "长".repeat(300));
        let out = split_html(&html, 100);
        assert!(!out.is_empty());
        assert!(out.iter().all(|c| visible_len(c) > 0), "{out:?}");
    }
}
