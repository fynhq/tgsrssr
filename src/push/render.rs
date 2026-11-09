//! Pure rendering and text planning for pushes: channel headers, entry
//! bodies/captions, source footers, rich-message HTML, and text chunking.
//! No async, DB, bot, or network calls belong here.

use feed_rs::model::Entry;

use crate::db;
use crate::fetcher;
use crate::htmltext::{
    escape_attr, escape_html, format_chat_id_html, format_chat_title_html, split_html,
    take_utf16_prefix, truncate_html_keep_links, utf16_len, visible_len,
};
use crate::i18n::Lang;
use crate::media::{MediaItem, MediaKind};

use super::{
    ContentMode, RenderOpts, SourceDisplay, ALBUM_LIMIT, CAPTION_LIMIT, RICH_MEDIA_MAX,
    RICH_TEXT_MAX, SAFE_LIMIT,
};

/// Smallest split budget we allow for an entry body. Without a floor, a
/// footer close to the message limit drives `split_html` down to one visible
/// character per message, producing a flood of tiny messages.
pub(super) const MIN_TEXT_CHUNK: usize = 512;
/// Display cap for feed titles/authors in the source footer. The stored value
/// stays untouched; this only bounds what the public channel sees.
pub(super) const SOURCE_DISPLAY_MAX: usize = 200;

/// (active, paused) subscription counts shared by the channel headers and
/// the resume-all button. Single definition of "active" for UI surfaces.
pub fn channel_sub_counts(subs: &[db::SubRow]) -> (usize, usize) {
    let active = subs.iter().filter(|s| s.active == 1).count();
    (active, subs.len() - active)
}

/// Shared channel-page header for /list (`list_header`) and the callback
/// channel page (`chpage_header`): both templates take the same five values
/// (title, chat link, total, active, paused), so the count logic and fill
/// live here and the two surfaces cannot drift.
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

// Hot-path regexes are compiled statically once (hit on every message/chunk).
// RE_TAG and the anchor/entity regexes live in crate::htmltext along with the
// HTML helpers (imported via the `use` at the top).
// Rate-limiting/backoff policy (pacer, SendRetryState, SendRetryPolicy)
// moved to crate::sendpolicy (imported via the `use` at the top).
// The Weibo poll data marker has been migrated to EntryMeta.poll (group-C
// sidecar); RE_VOTE_DIV has been removed.

/// Visible-length budget used by the standard text path.
///
/// `send_entry_msg` always reserves `footer + 2` chars for the footer and its
/// `\n\n` separator, even when the footer is empty. The effective single-
/// message body limit is therefore 4094, not 4096; a 4095-char text-only body
/// is already split by the standard path. Keep this helper aligned with that
/// reservation so the rich trigger never under-fires at the boundary.
pub(super) fn standard_text_len(body: &str, footer: &str) -> usize {
    visible_len(body) + visible_len(footer) + 2
}

/// Automatic rich-message enhancement, text-only path.
///
/// Only entries that would need more than one standard message are eligible;
/// normal entries keep the existing renderer unchanged.
pub(super) fn should_try_rich_text(text_len: usize) -> bool {
    text_len > SAFE_LIMIT && text_len <= RICH_TEXT_MAX
}

/// Automatic rich-message enhancement, media path.
///
/// `media_count` covers every attachment that enters the rich message:
/// photos, videos and documents (including oversized/long images converted to
/// documents and audio/file items). GIFs and retweet-only images are
/// deliberately excluded: they are still sent separately and are not part of
/// the 10-item cap.
pub(super) fn should_try_rich_media(text_len: usize, media_count: usize) -> bool {
    media_count > 0
        && media_count <= RICH_MEDIA_MAX
        && text_len <= RICH_TEXT_MAX
        && (media_count > ALBUM_LIMIT || text_len > CAPTION_LIMIT)
}

/// Compose rich HTML from the already-rendered Telegram HTML.
///
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

/// Rich HTML is parsed as an HTML document, so raw newlines are whitespace and
/// collapse (most visibly inside `<blockquote>`). Convert text newlines to
/// `<br>`; keep `<pre>` contents verbatim because pre preserves whitespace.
///
/// Block-level elements already start/end on their own line, so a newline run
/// adjacent to one contributes one fewer `<br>`; this keeps the visual newline
/// count identical to the standard parse_mode path.
fn preserve_rich_linebreaks(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut pre_depth = 0usize;
    let mut prev_block = false;
    let mut i = 0usize;
    while i < html.len() {
        let rest = &html[i..];
        if let Some(tag_len) = rest.strip_prefix('<').and_then(|r| r.find('>')).map(|p| p + 2) {
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

/// `caption_above_media` (the existing channel/source switch) decides whether
/// the text block precedes or follows the media block.
pub(super) fn rich_html(body: &str, footer: &str, media_html: &str, caption_above_media: bool) -> String {
    let mut text = body.to_string();
    if !footer.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(footer);
    }
    let text = preserve_rich_linebreaks(&text);

    if media_html.is_empty() {
        return text;
    }
    if text.is_empty() {
        return media_html.to_string();
    }
    // Media blocks are block-level (img / tg-collage / tg-slideshow), so no
    // raw newline separator is needed; a raw newline would be collapsed.
    if caption_above_media {
        format!("{text}{media_html}")
    } else {
        format!("{media_html}{text}")
    }
}

/// Build the rich HTML media blocks. Photos/videos use one attachment, a
/// 2–10 collage, or a >10 slideshow; documents follow as individual
/// `<tg-document>` blocks because collages/slideshows only accept photos and
/// videos. `album` and `docs` must stay in the same order as the
/// corresponding `InputRichMessage.media` entries (album ids first).
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
            // Media is already a block on its own; no figure wrapper is
            // needed without a caption.
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

/// Truncate display text to at most `max_units` UTF-16 units (Telegram's
/// measure), appending an ellipsis when content was dropped.
pub(super) fn truncate_display_text(s: &str, max_units: usize) -> String {
    if utf16_len(s) <= max_units {
        return s.to_string();
    }
    format!("{}…", take_utf16_prefix(s, max_units.saturating_sub(1)))
}

/// Source footer line, or empty string.
pub(super) fn source_line(entry: &Entry, feed_title: &str, author: Option<&str>, sd: SourceDisplay) -> String {
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

/// Max visible characters kept in a `Title+summary` excerpt.
pub(super) const SUMMARY_MAX_CHARS: usize = 140;
/// Raw HTML prefix converted for a summary. A summary only needs the first
/// 140 visible characters; converting a multi-megabyte body just to discard
/// it is a large transient allocation. The cut respects UTF-8 and tag
/// boundaries; when the prefix carries no visible text (e.g. a huge inline
/// image), the caller falls back to the full conversion.
const SUMMARY_INPUT_CAP: usize = 256 * 1024;

/// Truncate a raw HTML source to at most `cap` bytes without splitting a
/// UTF-8 code point or a tag (cut after the last complete `>` at/before the
/// cap). Plain text without any tag is cut directly; a malformed tag fragment
/// with no closing `>` yields an empty prefix so the caller can fall back.
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

/// Build a short summary from the entry body: the first `max_chars` visible
/// characters as Telegram-HTML with hyperlinks preserved, with "......"
/// appended when truncated. Returns empty string when the body has no
/// visible text.
pub(super) fn entry_summary(entry: &Entry, max_chars: usize, meta: Option<&crate::entrymeta::EntryMeta>) -> String {
    let source = fetcher::entry_body_source(entry);
    let capped = truncate_html_input(source, SUMMARY_INPUT_CAP);
    let mut html = fetcher::entry_body_html_from_source(capped, 100_000, meta);
    let (mut cut, mut truncated) = truncate_html_keep_links(&html, max_chars);
    if cut.trim().is_empty() && capped.len() < source.len() {
        // The prefix was all markup (e.g. a huge inline image); only the full
        // body can produce a summary.
        html = fetcher::entry_body_html(entry, 100_000, meta);
        (cut, truncated) = truncate_html_keep_links(&html, max_chars);
    }
    // Truncate on the Telegram-HTML the body pipeline already produced:
    // image media hrefs were dropped upstream, while video links are kept, so
    // surviving anchors are real content/video links (e.g. the WeChat article
    // behind "本文来自微信公众号：账号名") and stay clickable in the summary.
    // The cut counts display characters only — href text never eats into the
    // budget.
    // Whitespace at the edges comes from flattened block tags (the old
    // plain-text path trimmed the same way); an all-whitespace cut means the
    // body had no visible content.
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

/// Format an entry into (body_html, footer_html) separately.
/// The footer (source line) is returned separately so that when the message
/// is split into multiple parts, the footer can be placed on the LAST message.
/// Returns None for MediaOnly.
pub(super) fn format_entry_parts(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> Option<(String, String)> {
    render_parts(entry, feed_title, author, opts, meta)
        .map(|r| (r.parts.join("\n\n"), r.footer))
}

/// Shared rendering for the plain-text and caption paths. These were two
/// near-identical implementations that had already drifted once (TitleContent
/// silently lost the body on the media path), so the rules live here only.
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
            // Body is already Telegram-HTML (media stripped, links as <a>).
            // A large limit: splitting into multiple messages happens at send
            // time, so we never want to fall back to plain text here.
            let body = fetcher::entry_body_html(entry, 100_000, meta);
            if visible_len(&body) > 0 {
                parts.push(body);
            }
        }
        ContentMode::TitleOnly => parts.push(head(false)),
        ContentMode::ContentOnly => {
            let body = fetcher::entry_body_html(entry, 100_000, meta);
            if visible_len(&body) == 0 {
                // No-content fallback: degrade to a bold title to avoid an
                // empty message (both text and caption paths).
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

/// Build a media caption. All modes are platform-agnostic — Weibo gets no
/// special-casing here (Weibo sources are typically switched to ContentOnly
/// by the operator, which renders the post body directly).
///
/// - MediaOnly: only the source line.
/// - ContentOnly: show the body text directly (no synthesized title).
/// - Title modes: bold title (linked where applicable) + optional summary.
///
/// No truncation here — `send_entry_with_media` splits into a ≤1024 caption
/// plus follow-up text messages via `split_html`.
pub(super) fn format_caption(
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> String {
    let Some(rendered) = render_parts(entry, feed_title, author, opts, meta) else {
        return source_line(entry, feed_title, author, opts.source);
    };
    let mut parts = rendered.parts;
    if !rendered.footer.is_empty() {
        parts.push(rendered.footer);
    }
    parts.join("\n\n")
}

/// Plan the body chunks and the normalized footer for one entry.
///
/// The last chunk carries the footer, so its budget is reserved up front. The
/// footer itself is capped to leave at least `MIN_TEXT_CHUNK` visible units
/// for the body; this is what prevents a near-4096 footer from degenerating
/// into one-character chunks and a final message that exceeds Telegram's
/// limit. `visible_len` and `truncate_html_keep_links` now share the same
/// entity-decoded unit, so the reserve matches the truncation budget.
pub(super) fn plan_text_chunks(html_text: &str, footer: &str) -> (String, Vec<String>) {
    let body_len = visible_len(html_text);
    // An empty body still needs one unit of room so the "\n\n" separator does
    // not eat the footer budget; a non-empty body gets the full floor.
    let min_body = if body_len == 0 { 1 } else { MIN_TEXT_CHUNK };
    let footer_limit = SAFE_LIMIT.saturating_sub(2 + min_body);
    let footer = if visible_len(footer) > footer_limit {
        // HTML-aware: a raw UTF-16 cut can split an <a>/<i> tag or entity.
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

    // Defensive check: the budget math above should make this unreachable, but
    // never hand Telegram an over-limit last chunk (it would 400 and enter the
    // outbox retry loop).
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
