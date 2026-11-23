use feed_rs::model::Entry;

use crate::db;
use crate::fetcher::{self, TableMode};
use crate::htmltext::{
    escape_attr, escape_html, format_chat_id_html, format_chat_title_html,
    normalize_blockquote_fold, split_html, take_utf16_prefix, truncate_html_keep_links, utf16_len,
    visible_len,
};
use crate::i18n::Lang;
use crate::media::{MediaItem, MediaKind};

use super::{
    ContentMode, RenderOpts, SourceDisplay, ALBUM_LIMIT, CAPTION_LIMIT, RICH_MEDIA_MAX,
    RICH_TEXT_MAX, SAFE_LIMIT,
};

pub(super) const MIN_TEXT_CHUNK: usize = 512;
pub(super) const SOURCE_DISPLAY_MAX: usize = 200;

pub fn channel_sub_counts(subs: &[db::SubRow]) -> (usize, usize) {
    let active = subs.iter().filter(|s| s.active == 1).count();
    (active, subs.len() - active)
}

pub fn channel_header_text(
    lang: Lang,
    template_key: &str,
    title: &str,
    chat_id: &str,
    subs: &[db::SubRow],
) -> String {
    let (active_count, paused_count) = channel_sub_counts(subs);
    let title_html = format_chat_title_html(title, chat_id);
    let chat_html = format_chat_id_html(chat_id);
    let total_s = subs.len().to_string();
    let active_s = active_count.to_string();
    let paused_s = paused_count.to_string();
    crate::i18n::fill(
        crate::i18n::t(lang, template_key),
        &[&title_html, &chat_html, &total_s, &active_s, &paused_s],
    )
}

// The standard text path reserves footer + 2, so the effective body limit is 4094; keep the rich trigger aligned.
pub(super) fn standard_text_len(body: &str, footer: &str) -> usize {
    visible_len(body) + visible_len(footer) + 2
}

pub(super) fn should_try_rich_text(text_len: usize) -> bool {
    text_len > SAFE_LIMIT && text_len <= RICH_TEXT_MAX
}

pub(super) fn should_try_rich_media(text_len: usize, media_count: usize) -> bool {
    media_count > 0
        && media_count <= RICH_MEDIA_MAX
        && text_len <= RICH_TEXT_MAX
        && (media_count > ALBUM_LIMIT || text_len > CAPTION_LIMIT)
}

fn rich_tag_name(tag: &str) -> String {
    tag.trim()
        .trim_start_matches('<')
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_rich_block_tag_name(name: &str) -> bool {
    matches!(
        name,
        "blockquote"
            | "pre"
            | "p"
            | "div"
            | "hr"
            | "figure"
            | "figcaption"
            | "img"
            | "video"
            | "audio"
            | "tg-collage"
            | "tg-slideshow"
            | "tg-document"
            | "tg-map"
            | "tg-button-row"
            | "footer"
            | "aside"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "table"
            | "thead"
            | "tbody"
            | "tr"
            | "td"
            | "th"
            | "details"
            | "summary"
    )
}

fn is_rich_block_tag(tag: &str) -> bool {
    is_rich_block_tag_name(&rich_tag_name(tag))
}

fn next_tag_is_rich_block(html: &str, mut pos: usize) -> bool {
    while pos < html.len() {
        let b = html.as_bytes()[pos];
        if b == b' ' || b == b'\t' {
            pos += 1;
        } else {
            break;
        }
    }
    if pos >= html.len() || html.as_bytes()[pos] != b'<' {
        return false;
    }
    let Some(end) = html[pos..].find('>') else {
        return false;
    };
    is_rich_block_tag(&html[pos..pos + end + 1])
}

fn preserve_rich_linebreaks(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut pre_depth = 0usize;
    let mut prev_block = false;
    let mut i = 0usize;
    while i < html.len() {
        let rest = &html[i..];
        if let Some(tag_len) = rest
            .strip_prefix('<')
            .and_then(|r| r.find('>'))
            .map(|p| p + 2)
        {
            let tag = &html[i..i + tag_len];
            let trimmed = tag.trim();
            let closing = trimmed.starts_with("</");
            let self_closing = trimmed.ends_with("/>");
            let name = rich_tag_name(tag);
            let is_block = is_rich_block_tag_name(&name);
            out.push_str(tag);
            if name == "pre" && !self_closing {
                if closing {
                    pre_depth = pre_depth.saturating_sub(1);
                } else {
                    pre_depth += 1;
                }
            }
            prev_block = is_block;
            i += tag_len;
            continue;
        }

        if pre_depth > 0 {
            let c = rest.chars().next().expect("non-empty rest");
            if c == '\r' && rest[1..].starts_with('\n') {
                out.push_str("\r\n");
                i += 2;
            } else {
                out.push(c);
                i += c.len_utf8();
            }
            continue;
        }

        let mut count = 0usize;
        let mut j = i;
        while j < html.len() {
            let tail = &html[j..];
            if tail.starts_with("\r\n") {
                count += 1;
                j += 2;
            } else if tail.starts_with('\n') || tail.starts_with('\r') {
                count += 1;
                j += 1;
            } else {
                break;
            }
        }
        if count > 0 {
            let next_block = next_tag_is_rich_block(html, j);
            let subtract = usize::from(prev_block || next_block);
            for _ in 0..count.saturating_sub(subtract) {
                out.push_str("<br>");
            }
            prev_block = false;
            i = j;
            continue;
        }

        let c = rest.chars().next().expect("non-empty rest");
        out.push(c);
        if !c.is_whitespace() {
            prev_block = false;
        }
        i += c.len_utf8();
    }
    out
}

pub(super) fn rich_html(
    body: &str,
    footer: &str,
    media_html: &str,
    caption_above_media: bool,
) -> String {
    let mut text = body.to_string();
    if !footer.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(footer);
    }
    // Fold before newline conversion: the header rule scans for literal blank lines.
    let text = preserve_rich_linebreaks(&normalize_blockquote_fold(&text));

    if media_html.is_empty() {
        return text;
    }
    if text.is_empty() {
        return media_html.to_string();
    }
    if caption_above_media {
        format!("{text}{media_html}")
    } else {
        format!("{media_html}{text}")
    }
}

pub(super) fn rich_media_html(album: &[&MediaItem], docs: &[&MediaItem]) -> String {
    let mut html = String::new();

    if !album.is_empty() {
        let items: Vec<String> = album
            .iter()
            .enumerate()
            .map(|(i, m)| match m.kind {
                MediaKind::Video => format!("<video src=\"tg://video?id=m{i}\"></video>"),
                _ => format!("<img src=\"tg://photo?id=m{i}\"/>"),
            })
            .collect();
        if items.len() == 1 {
            html.push_str(&items[0]);
        } else if items.len() <= ALBUM_LIMIT {
            html.push_str(&format!("<tg-collage>{}</tg-collage>", items.concat()));
        } else {
            html.push_str(&format!("<tg-slideshow>{}</tg-slideshow>", items.concat()));
        }
    }

    for (i, _) in docs.iter().enumerate() {
        html.push_str(&format!(
            "<tg-document src=\"tg://document?id=m{}\"></tg-document>",
            album.len() + i
        ));
    }

    html
}

pub(super) fn truncate_display_text(s: &str, max_units: usize) -> String {
    if utf16_len(s) <= max_units {
        return s.to_string();
    }
    format!("{}…", take_utf16_prefix(s, max_units.saturating_sub(1)))
}

pub(super) fn source_line(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    sd: SourceDisplay,
) -> String {
    match sd {
        SourceDisplay::Source => {
            if feed_title.is_empty() {
                return String::new();
            }
            let title = truncate_display_text(feed_title, SOURCE_DISPLAY_MAX);
            match fetcher::entry_link(entry) {
                Some(url) => format!(
                    "<i>via <a href=\"{}\">{}</a></i>",
                    escape_attr(&url),
                    escape_html(&title)
                ),
                None => format!("<i>via {}</i>", escape_html(&title)),
            }
        }
        SourceDisplay::Author => match author {
            Some(a) if !a.is_empty() => format!(
                "<i>author: {}</i>",
                escape_html(&truncate_display_text(a, SOURCE_DISPLAY_MAX))
            ),
            _ => String::new(),
        },
        SourceDisplay::None => String::new(),
    }
}

pub(super) const SUMMARY_MAX_CHARS: usize = 140;
const SUMMARY_INPUT_CAP: usize = 256 * 1024;

pub(super) fn truncate_html_input(html: &str, cap: usize) -> &str {
    if html.len() <= cap {
        return html;
    }
    let mut end = cap;
    while end > 0 && !html.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &html[..end];
    match prefix.rfind('>') {
        Some(i) => &html[..=i],
        None if !prefix.contains('<') => prefix,
        None => "",
    }
}

pub(super) fn entry_summary(
    entry: &Entry,
    max_chars: usize,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> String {
    let source = fetcher::entry_body_source(entry);
    let capped = truncate_html_input(source, SUMMARY_INPUT_CAP);
    let mut html = fetcher::entry_body_html_from_source(capped, 100_000, meta);
    let (mut cut, mut truncated) = truncate_html_keep_links(&html, max_chars);
    if cut.trim().is_empty() && capped.len() < source.len() {
        html = fetcher::entry_body_html(entry, 100_000, meta);
        (cut, truncated) = truncate_html_keep_links(&html, max_chars);
    }
    let cut = cut.trim();
    if cut.is_empty() {
        return String::new();
    }
    if truncated {
        format!("{}......", cut)
    } else {
        cut.to_string()
    }
}

pub(super) fn format_entry_parts(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> Option<(String, String)> {
    format_entry_parts_with_mode(entry, feed_title, author, opts, meta, TableMode::Flatten)
}

pub(super) fn format_entry_parts_with_mode(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
    table_mode: TableMode,
) -> Option<(String, String)> {
    render_parts(entry, feed_title, author, opts, meta, table_mode)
        .map(|r| (r.parts.join("\n\n"), r.footer))
}

struct RenderedParts {
    parts: Vec<String>,
    footer: String,
}

fn render_parts(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
    table_mode: TableMode,
) -> Option<RenderedParts> {
    let title = fetcher::entry_title(entry, opts.lang);
    let link = fetcher::entry_link(entry);
    let head = |linked: bool| -> String {
        match (&link, linked) {
            (Some(url), true) => format!(
                "<b><a href=\"{}\">{}</a></b>",
                escape_attr(url),
                escape_html(&title)
            ),
            _ => format!("<b>{}</b>", escape_html(&title)),
        }
    };

    let mut parts: Vec<String> = Vec::new();
    match opts.mode {
        ContentMode::MediaOnly => return None,
        ContentMode::TitleLink => parts.push(head(true)),
        ContentMode::TitleSummary => {
            parts.push(head(true));
            let summary = entry_summary(entry, SUMMARY_MAX_CHARS, meta);
            if !summary.is_empty() {
                parts.push(summary);
            }
        }
        ContentMode::TitleContent => {
            parts.push(head(true));
            let body = fetcher::entry_body_html_with_mode(entry, 100_000, meta, table_mode);
            if visible_len(&body) > 0 {
                parts.push(body);
            }
        }
        ContentMode::TitleOnly => parts.push(head(false)),
        ContentMode::ContentOnly => {
            let body = fetcher::entry_body_html_with_mode(entry, 100_000, meta, table_mode);
            if visible_len(&body) == 0 {
                parts.push(head(false));
            } else {
                parts.push(body);
            }
        }
    }
    Some(RenderedParts {
        parts,
        footer: source_line(entry, feed_title, author, opts.source),
    })
}

pub(super) fn format_caption(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> String {
    format_caption_with_mode(entry, feed_title, author, opts, meta, TableMode::Flatten)
}

pub(super) fn format_caption_with_mode(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
    table_mode: TableMode,
) -> String {
    let Some(rendered) = render_parts(entry, feed_title, author, opts, meta, table_mode) else {
        return source_line(entry, feed_title, author, opts.source);
    };
    let mut parts = rendered.parts;
    if !rendered.footer.is_empty() {
        parts.push(rendered.footer);
    }
    parts.join("\n\n")
}

pub(super) fn plan_text_chunks(html_text: &str, footer: &str) -> (String, Vec<String>) {
    let body_len = visible_len(html_text);
    let min_body = if body_len == 0 { 1 } else { MIN_TEXT_CHUNK };
    let footer_limit = SAFE_LIMIT.saturating_sub(2 + min_body);
    let footer = if visible_len(footer) > footer_limit {
        let (cut, _) = truncate_html_keep_links(footer, footer_limit.saturating_sub(1));
        format!("{cut}…")
    } else {
        footer.to_string()
    };
    let footer_reserve = visible_len(&footer) + 2;
    let limit = SAFE_LIMIT
        .saturating_sub(footer_reserve)
        .max(MIN_TEXT_CHUNK);
    let mut chunks = split_html(html_text, limit);
    if chunks.is_empty() {
        chunks.push(html_text.to_string());
    }

    let last_budget = SAFE_LIMIT.saturating_sub(footer_reserve);
    if let Some(last) = chunks.last_mut() {
        if visible_len(last) > last_budget {
            tracing::warn!(
                chunk_units = visible_len(last),
                budget = last_budget,
                "last text chunk exceeds the footer-adjusted budget; truncating"
            );
            let (cut, _) = truncate_html_keep_links(last, last_budget.saturating_sub(1));
            *last = format!("{cut}…");
        }
    }
    (footer, chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_html_keeps_table_markup_compact() {
        let table =
            "<table bordered><tr><td>A</td><td>B</td></tr><tr><td>1</td><td>2</td></tr></table>";
        let out = rich_html(&format!("before\n{table}\nafter"), "", "", true);
        assert!(out.contains(table), "out: {out}");
        assert!(!out.contains("<td><br>"), "out: {out}");
        assert!(!out.contains("</td><br>"), "out: {out}");
        assert!(!out.contains("<br><table"), "out: {out}");
    }

    #[test]
    fn rich_html_folds_short_blockquote_header_before_linebreaks() {
        let body = format!("<blockquote>header\n\n{}</blockquote>", "x".repeat(300));
        let out = rich_html(&body, "", "", true);
        assert!(out.contains("header<br>x"), "out: {out}");
        assert!(out.contains("<blockquote expandable>"), "out: {out}");
    }
}
