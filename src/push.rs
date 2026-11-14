use feed_rs::model::Entry;
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, InputFile, InputMedia, InputMediaDocument, InputMediaPhoto, InputMediaVideo, Message,
    ParseMode,
};

use crate::db::{self, Db};
use crate::fetcher;
use crate::htmltext::{merge_adjacent_blockquotes, split_html};
use crate::i18n::Lang;
use crate::media::{MediaItem, MediaKind};
use crate::richmsg::{self, RichMedia, RichMediaKind, RichMediaSource};
use crate::sendpolicy::{
    error_chain_string, pace, SendRetryAction, SendRetryPolicy, SendRetryState,
};

// Re-export pure HTML helpers for external callers (commands, callbacks,
// scheduler, platforms). `pub use` also brings them into push.rs scope.
pub use crate::htmltext::{
    escape_attr, escape_html, format_chat_id_html, format_chat_title_html,
    normalize_blockquote_fold, take_utf16_prefix, utf16_len, visible_len,
};

mod render;
pub use render::{channel_header_text, channel_sub_counts};
use render::{
    format_caption, format_entry_parts, plan_text_chunks, rich_html, rich_media_html,
    should_try_rich_media, should_try_rich_text, standard_text_len,
};

/// Leave room for suffixes. Telegram's hard limit is 4096 visible chars;
/// the footer (source line) is appended to the last chunk, and the split
/// budget reserves the footer's visible length + 2 separator chars up front
/// (see send_entry_msg), so staying within this limit keeps the combined
/// text under 4096.
const SAFE_LIMIT: usize = 4096;
/// Telegram caption limit for media messages.
const CAPTION_LIMIT: usize = 1024;
/// Bot API rich message hard limits (10.x): rich text and media attachments.
const RICH_TEXT_MAX: usize = 32768;
const RICH_MEDIA_MAX: usize = 50;
/// Safety cap for rich attachments uploaded through multipart. Photos/videos
/// use `file://` on the local Bot API and do not count; documents (and all
/// media on the official API) stream from disk and therefore do not consume
/// equivalent heap memory either. The cap is kept as a deliberate payload
/// threshold: very large sets fall back to the standard album path, matching
/// the established rich-message behavior.
const RICH_MULTIPART_MEDIA_MAX: u64 = 128 * 1024 * 1024;
/// The existing album chunk size; > this triggers the rich-media enhancement.
const ALBUM_LIMIT: usize = 10;

/// Shared constructor that disables link preview (used by command responses,
/// notifications, and text messages).
/// Commands/notifications often contain source links and message links; the
/// preview cards are noise.
pub fn no_preview() -> teloxide::types::LinkPreviewOptions {
    teloxide::types::LinkPreviewOptions {
        is_disabled: true,
        url: None,
        prefer_small_media: false,
        prefer_large_media: false,
        show_above_text: false,
    }
}

/// Retire a pending-op prompt by editing it into a plain status line.
/// Editing (instead of deleting) keeps the reply target alive so the
/// client's local reply-draft — and the quote anchor of the user's reply —
/// never dangles on a deleted message.
///
/// Prompts are sent as plain messages without any reply markup (ForceReply
/// was removed in v1.3.2: its markup could not be reliably cleared via
/// editMessageText and left clients stuck in a permanent reply state), so a
/// single text edit suffices. If the edit fails (e.g. message too old) the
/// message is left in place — never deleted.
///
/// Defined in push.rs (breaks the callbacks→commands reverse dependency
/// edge); commands keeps a re-export to preserve existing call
/// paths.
pub async fn retire_prompt(
    bot: &Bot,
    chat: teloxide::types::ChatId,
    prompt_msg_id: i32,
    status: &str,
) {
    let mid = teloxide::types::MessageId(prompt_msg_id);
    if bot.edit_message_text(chat, mid, status).await.is_err() {
        // Truly uneditable (e.g. too old). Leave it in place — deleting would
        // remove the very message the user's reply quotes.
        tracing::warn!(
            chat_id = chat.0,
            message_id = prompt_msg_id,
            "retire_prompt: edit failed, leaving message in place (no delete)"
        );
    }
}

/// Outcome of sending one entry.
/// - `media_failed`: true when media could not be sent (fell back to text,
///   or a document send failed) — the caller reports this privately to
///   the chat owner, never to the channel.
/// - `entry_link`: the source entry URL (e.g. the Weibo post).
/// - `delivered_tg_link`: t.me link of the message actually delivered (the
///   main media message / text fallback), used in owner notices and for
///   linking download warnings back to the channel.
/// - `delivered_message_id`: numeric message id of the delivered message,
///   used to register the TG-message → RSS-entry mapping for /find.
#[derive(Debug, Clone, Default)]
pub struct SendOutcome {
    pub media_failed: bool,
    pub entry_link: Option<String>,
    pub delivered_tg_link: Option<String>,
    pub delivered_message_id: Option<i64>,
    /// The entry's main message is the plain-text fallback (including the
    /// text-only path and "all media failed -> send full text"). Used by the
    /// oversized-video notice so it does not claim a text message when the
    /// entry was actually delivered with other media.
    pub text_only: bool,
    /// The rich-message request may have been delivered even though the
    /// response failed (timeout / connection reset / interrupted body / 5xx).
    /// Callers must mark the entry sent and notify the owner instead of
    /// falling back or queueing an outbox retry (A1 policy).
    pub uncertain: bool,
    /// The first text chunk was delivered but a later chunk failed. The entry
    /// is marked sent and never sent to the outbox (a retry would replay the
    /// delivered prefix); the owner is told that the tail is missing.
    pub text_partial: bool,
}

/// Extract Weibo poll data from EntryMeta (sidecar explicit protocol,
/// replacing the former in-body data-vote marker parsing). Returns
/// (question, options, is_multi_select).
/// Telegram limits: question ≤300 chars, options 2-10 items of ≤100 chars each.
pub fn extract_poll(
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> Option<(String, Vec<String>, bool)> {
    let poll = meta.and_then(|m| m.poll.as_ref())?;
    truncate_poll(&poll.question, &poll.options, poll.is_multi_select)
}

/// Truncate to Telegram poll limits: question ≤300, options ≤10 items of
/// ≤100 chars each.
/// Counted in UTF-16 code units (Telegram's measure; emoji count as 2 units),
/// consistent with message splitting (#3 fix).
fn truncate_poll(
    question: &str,
    options: &[String],
    is_multi: bool,
) -> Option<(String, Vec<String>, bool)> {
    // Guard: an empty question is never turned into a poll (weibo.rs already
    // blocks this at construction time; this defends against outbox
    // deserialization or future data bypassing the construction-side check).
    if question.trim().is_empty() {
        return None;
    }
    let question = question.to_string();
    let question = if utf16_len(&question) > 300 {
        format!("{}...", take_utf16_prefix(&question, 297))
    } else {
        question
    };
    let options: Vec<String> = options
        .iter()
        .take(10)
        .map(|o| {
            if utf16_len(o) > 100 {
                format!("{}...", take_utf16_prefix(o, 97))
            } else {
                o.clone()
            }
        })
        .collect();
    if options.len() < 2 {
        return None;
    }
    Some((question, options, is_multi))
}

/// Send a Telegram poll (sendPoll). On failure only a warn log is emitted;
/// the entry push is not affected.
/// The poll replies to the main message (reply_to) so it stays associated
/// within the channel.
/// Always anonymous: Telegram forbids non-anonymous polls in channels
/// (Bad Request: non-anonymous polls can't be sent to channel chats).
/// After a successful send the poll message is pinned automatically
/// (following the channel notification options, allowed to replace an older
/// pin).
/// Paced before sending; flood and transient retries are driven by the
/// shared SendRetryState policy (#4 fix).
pub async fn send_poll(
    bot: &Bot,
    chat_id: i64,
    question: &str,
    options: &[String],
    is_multi: bool,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    use teloxide::requests::Requester;
    use teloxide::types::InputPollOption;
    let poll_options: Vec<InputPollOption> = options
        .iter()
        .map(|o| InputPollOption {
            text: o.clone(),
            formatting: None,
        })
        .collect();
    pace(chat_id).await;
    let mut retry = SendRetryState::new(SendRetryPolicy::POLL, false, false);
    let msg = loop {
        let mut req = bot
            .send_poll(ChatId(chat_id), question, poll_options.clone())
            .is_anonymous(true)
            .allows_multiple_answers(is_multi);
        if let Some(mid) = reply_to {
            req = req.reply_parameters(teloxide::types::ReplyParameters::new(mid));
        }
        match req.await {
            Ok(m) => break m,
            Err(e) => {
                let es = error_chain_string(&e);
                match retry.next_action(&es) {
                    SendRetryAction::RetryFlood { delay } => {
                        tracing::warn!(chat_id = chat_id, wait_secs = delay.as_secs(), error = %es, "poll send flood control, backing off");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    // v1.10.2: retry on network/TG transient errors
                    // (consistent with the other send sites)
                    SendRetryAction::RetryTransient { delay } => {
                        tracing::warn!(chat_id = chat_id, backoff = delay.as_secs(), error = %es, "poll send transient error, backing off");
                        tokio::time::sleep(delay).await;
                        pace(chat_id).await;
                        continue;
                    }
                    _ => return Err(e.into()),
                }
            }
        }
    };
    // Pin the poll message: on failure only warn (e.g. missing permission);
    // the poll itself is not affected.
    if let Err(e) = bot.pin_chat_message(ChatId(chat_id), msg.id).await {
        tracing::warn!(chat_id = chat_id, error = %e, "pin poll message failed (missing permission?)");
    }
    Ok(msg)
}

/// Unified follow-up poll helper for Weibo polls (extracted in v1.9.7):
/// after the body is delivered, try to extract and send the poll.
/// Returns whether the poll send failed (so the caller can set media_failed
/// and notify the owner).
/// All send paths (plain text / media / media-failure fallback / outbox
/// retry) go through this single function, preventing another missed-send
/// caused by copy-paste drift (the root cause fixed in v1.9.6 was exactly a
/// missing call on the plain-text path).
pub async fn try_send_poll(
    bot: &Bot,
    chat_id: i64,
    reply_to: Option<teloxide::types::MessageId>,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> bool {
    if let Some((question, options, is_multi)) = extract_poll(meta) {
        if let Err(e) = send_poll(bot, chat_id, &question, &options, is_multi, reply_to).await {
            tracing::warn!(chat_id = chat_id, error = %e, "poll send failed");
            return true;
        }
    }
    false
}

/// Unified helper for the plain-text send path. The scheduler main push and
/// the outbox retry previously duplicated
/// format_entry_parts→send_entry_msg→try_send_poll→SendOutcome verbatim,
/// and the poll follow-up logic had been fixed twice because of it
/// (v1.9.3/v1.9.6); converged into a single function to prevent further
/// drift).
/// Returns Ok(SendOutcome) or Err (the caller decides between outbox and
/// fallback). Text longer than the standard 4096 limit gets one rich-message
/// attempt first; any failure falls through to the existing split path.
pub async fn send_text_only(
    bot: &Bot,
    chat_id: i64,
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> anyhow::Result<SendOutcome> {
    let Some((body, footer)) = format_entry_parts(entry, feed_title, author, opts, meta) else {
        return Ok(SendOutcome::default());
    };
    // Includes the 2-char footer/separator reserve used by send_entry_msg,
    // so a 4095-char body with no footer correctly triggers the rich path
    // instead of being split by the standard one.
    let text_len = standard_text_len(&body, &footer);
    tracing::debug!(
        chat_id = chat_id,
        entry_id = %fetcher::entry_id(entry),
        text_len = text_len,
        allow_rich = opts.allow_rich,
        rich_eligible = opts.allow_rich
            && !crate::richmsg::is_unsupported()
            && should_try_rich_text(text_len),
        "/push: text path decision"
    );
    let mut rich_msg: Option<Message> = None;
    if opts.allow_rich && !crate::richmsg::is_unsupported() && should_try_rich_text(text_len) {
        match try_send_rich_text(bot, chat_id, &body, &footer).await {
            Ok(m) => {
                tracing::info!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    text_len = text_len,
                    "rich text enhancement sent"
                );
                rich_msg = Some(m);
            }
            Err(e) => {
                let err_str = error_chain_string(e.as_ref());
                crate::richmsg::note_send_error(&err_str);
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    text_len = text_len,
                    error = %err_str,
                    "rich text send failed"
                );
                // A1 policy: an ambiguous outcome (timeout / reset / read
                // interruption / 5xx) may already be delivered. Do not resend
                // through the standard path; report uncertain so the caller
                // marks the entry sent and asks the owner to confirm.
                if crate::sendpolicy::is_ambiguous_send_error(&err_str) {
                    tracing::warn!(
                        chat_id = chat_id,
                        entry_id = %fetcher::entry_id(entry),
                        "rich text outcome uncertain; not falling back to standard path"
                    );
                    return Ok(SendOutcome {
                        entry_link: fetcher::entry_link(entry),
                        text_only: true,
                        uncertain: true,
                        ..SendOutcome::default()
                    });
                }
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    "rich text failure is explicitly undelivered, falling back to standard path"
                );
            }
        }
    }
    let (m, text_partial) = match rich_msg {
        Some(m) => (m, false),
        None => {
            let sent = send_entry_msg(bot, chat_id, &body, &footer, opts.link_preview).await?;
            (sent.first, sent.partial)
        }
    };
    let poll_failed = try_send_poll(bot, chat_id, Some(m.id), meta).await;
    Ok(SendOutcome {
        media_failed: poll_failed,
        entry_link: fetcher::entry_link(entry),
        delivered_tg_link: tg_message_link(&m),
        delivered_message_id: Some(m.id.0 as i64),
        text_only: true,
        uncertain: false,
        text_partial,
    })
}

/// Content mode for rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContentMode {
    /// Title (with hyperlink)
    TitleLink,
    /// Title (with hyperlink) + summary (first 50 chars of body + "......")
    TitleSummary,
    /// Title (with hyperlink) + content (truncated when too long)
    TitleContent,
    /// Title only (no link)
    TitleOnly,
    /// Content only (no title; fits title-less sources such as Weibo /
    /// Telegram channels)
    ContentOnly,
    /// Media only (silently skipped when there is no media)
    MediaOnly,
}

impl ContentMode {
    pub fn from_str(s: &str) -> Self {
        // Accept the legacy camel-case spellings used by older schema
        // defaults ('TitleSummary') in addition to the canonical snake_case.
        match s.to_ascii_lowercase().as_str() {
            "title_summary" | "titlesummary" => Self::TitleSummary,
            "title_content" | "titlecontent" => Self::TitleContent,
            "title_only" | "titleonly" => Self::TitleOnly,
            "content_only" | "contentonly" => Self::ContentOnly,
            "media_only" | "mediaonly" => Self::MediaOnly,
            _ => Self::TitleLink,
        }
    }

    /// Whether this mode skips media fetching entirely (title+link /
    /// title-only: push text only, never fetch media).
    /// The scheduler (timed pushes) and /test (commands) share this check to
    /// avoid rule drift between the two sites.
    pub fn skip_media(&self) -> bool {
        matches!(self, ContentMode::TitleLink | ContentMode::TitleOnly)
    }

    /// Whether this mode keeps only the first media item (title_summary).
    /// At fetch time `prepare_entry_media` stops after the first successfully
    /// downloaded image/video (audio/documents and GIFs are never fetched);
    /// the slice in `send_entry_with_media` stays as a compatibility guard
    /// for outbox rows serialized by older versions (they can still carry a
    /// full media set). Shared by scheduler and /test to avoid rule drift.
    pub fn single_media(&self) -> bool {
        matches!(self, ContentMode::TitleSummary)
    }
}

/// Source footer display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceDisplay {
    /// "via <source title>" (the source title carries the entry hyperlink)
    Source,
    /// "author: <author>" (no link)
    Author,
    /// Not shown
    None,
}

impl SourceDisplay {
    pub fn from_str(s: &str) -> Self {
        // Accept legacy camel-case spellings from old schema defaults, same
        // tolerance as ContentMode::from_str.
        match s.to_ascii_lowercase().as_str() {
            "author" => Self::Author,
            "none" => Self::None,
            _ => Self::Source,
        }
    }
}

/// Rendering options for one push.
#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub mode: ContentMode,
    pub source: SourceDisplay,
    pub link_preview: bool,
    /// Owner language for user-visible fallbacks (missing title, etc.).
    pub lang: Lang,
    /// Allow the automatic rich-message enhancement. Scheduler and /test pass
    /// true; outbox retries pass false so a rich failure never repeats.
    pub allow_rich: bool,
}

/// Prepare one local file for rich-media upload. Local Bot API gets a
/// `file://` path in its own namespace; otherwise the file is uploaded as a
/// streamed multipart part (opened at send time, never read into a Vec).
async fn rich_source_for_path(
    path: &std::path::Path,
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<RichMediaSource> {
    if let Some(bp) = transport.bot_path(path) {
        return Ok(RichMediaSource::FilePath(bp));
    }
    // Validate readability up front: callers use an error to omit a broken
    // thumbnail or fall back to the standard send path before building the
    // whole request.
    tokio::fs::metadata(path).await?;
    Ok(RichMediaSource::File {
        path: path.to_path_buf(),
        filename: crate::media::logical_file_name(path),
    })
}

/// Reuse the standard media file_id cache when possible, then fall back to the
/// same transport rules as the normal media path.
async fn rich_source_for_item(
    db: &Db,
    item: &MediaItem,
    transport: &crate::media::MediaTransport,
    mode: &str,
) -> anyhow::Result<RichMediaSource> {
    let key = format!("{}:{}", item.content_hash, mode);
    if let Ok(Some(fid)) = db::get_cached_file_id(db, &key).await {
        return Ok(RichMediaSource::FileId(fid));
    }
    rich_source_for_path(&item.path, transport).await
}

/// Document attachments must keep the logical file name in Telegram. A local
/// Bot API `file://` upload takes the name from the on-disk path, which still
/// carries the `.tgsr` storage suffix, so documents always go through
/// multipart with the stripped name. The file itself is streamed at send
/// time; the file_id cache avoids the upload entirely after the first send.
async fn rich_source_for_doc(db: &Db, item: &MediaItem) -> anyhow::Result<RichMediaSource> {
    let key = format!("{}:doc", item.content_hash);
    if let Ok(Some(fid)) = db::get_cached_file_id(db, &key).await {
        return Ok(RichMediaSource::FileId(fid));
    }
    tokio::fs::metadata(&item.path).await?;
    Ok(RichMediaSource::File {
        path: item.path.clone(),
        filename: crate::media::logical_file_name(&item.path),
    })
}

async fn rich_thumbnail_source(
    item: &MediaItem,
    transport: &crate::media::MediaTransport,
) -> Option<RichMediaSource> {
    let path = item.thumb_path.as_ref()?;
    match rich_source_for_path(path, transport).await {
        Ok(source) => Some(source),
        Err(e) => {
            tracing::warn!(error = %e, "rich media thumbnail preparation failed, sending without thumbnail");
            None
        }
    }
}

/// Build the `InputRichMessage.media` array. Order must match
/// `rich_media_html`: album photos/videos first, then documents.
async fn build_rich_media_specs(
    db: &Db,
    album: &[&MediaItem],
    docs: &[&MediaItem],
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<Vec<RichMedia>> {
    let mut out = Vec::with_capacity(album.len() + docs.len());

    for (i, item) in album.iter().enumerate() {
        let id = format!("m{i}");
        let (kind, mode) = match item.kind {
            MediaKind::Video => (RichMediaKind::Video, "video"),
            _ => (RichMediaKind::Photo, "photo"),
        };
        let source = rich_source_for_item(db, item, transport, mode).await?;
        let thumbnail = if kind == RichMediaKind::Video {
            rich_thumbnail_source(item, transport).await
        } else {
            None
        };
        out.push(RichMedia {
            id,
            kind,
            source,
            thumbnail,
            width: item.width,
            height: item.height,
            duration: item.duration_secs,
            disable_content_type_detection: false,
        });
    }

    for (i, item) in docs.iter().enumerate() {
        let id = format!("m{}", album.len() + i);
        let source = rich_source_for_doc(db, item).await?;
        let thumbnail = rich_thumbnail_source(item, transport).await;
        out.push(RichMedia {
            id,
            kind: RichMediaKind::Document,
            source,
            thumbnail,
            width: 0,
            height: 0,
            duration: None,
            // Image documents (WebP in particular) must stay documents so
            // they are not re-detected as stickers.
            disable_content_type_detection: item.kind == MediaKind::Image,
        });
    }

    Ok(out)
}

/// Single rich-message attempt. The rich path is an enhancement: any failure
/// (including 429) falls back to the standard path, which owns the full
/// flood/backoff/resume policy. Keeping this one-shot also avoids cloning
/// large in-memory media just to retry.
async fn send_rich_once(
    bot: &Bot,
    chat_id: i64,
    html: String,
    media: Vec<RichMedia>,
) -> anyhow::Result<Message> {
    pace(chat_id).await;
    richmsg::send_rich_message(bot, chat_id, html, media, None).await
}

async fn try_send_rich_text(
    bot: &Bot,
    chat_id: i64,
    body: &str,
    footer: &str,
) -> anyhow::Result<Message> {
    // Rich HTML supports the same expandable blockquote tag, so keep the
    // existing >200 visible-char folding rule from the standard path.
    let html = normalize_blockquote_fold(&rich_html(body, footer, "", true));
    send_rich_once(bot, chat_id, html, Vec::new()).await
}

// The parameters all share the push context; a one-off struct would only move
// the list, matching the send_rich_ops allowance below.
#[allow(clippy::too_many_arguments)]
async fn try_send_rich_media(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    text: &str,
    caption_above_media: bool,
    album_media: &[&MediaItem],
    docs: &[&MediaItem],
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<Message> {
    let media_count = album_media.len() + docs.len();
    if media_count == 0 {
        anyhow::bail!("rich media enhancement called without media");
    }
    if media_count > RICH_MEDIA_MAX {
        anyhow::bail!("too many rich media attachments: {media_count}");
    }
    if visible_len(text) > RICH_TEXT_MAX {
        anyhow::bail!("rich message text too long: {}", visible_len(text));
    }
    // Documents always upload as bytes (their file name must not carry the
    // `.tgsr` suffix), so their size counts even with a local Bot API.
    let multipart_bytes: u64 = album_media
        .iter()
        .filter(|m| transport.bot_path(&m.path).is_none())
        .map(|m| m.size)
        .sum::<u64>()
        + docs.iter().map(|m| m.size).sum::<u64>();
    if multipart_bytes > RICH_MULTIPART_MEDIA_MAX {
        anyhow::bail!(
            "rich media multipart payload too large: {} bytes (cap {})",
            multipart_bytes,
            RICH_MULTIPART_MEDIA_MAX
        );
    }

    let media_html = rich_media_html(album_media, docs);
    let html = normalize_blockquote_fold(&rich_html(text, "", &media_html, caption_above_media));
    let media = build_rich_media_specs(db, album_media, docs, transport).await?;
    send_rich_once(bot, chat_id, html, media).await
}

/// Build the InputFile for a local file path.
///
/// The file is uploaded by this process as a streaming multipart part
/// (teloxide reads it from disk chunk by chunk), so the heap never holds the
/// whole file. The bot-api namespace path is only used by rich-message
/// `file://` attachments, not by the standard send path.
///
/// A pre-open validates existence/readability up front: callers that treat a
/// failure as "skip this attachment" (thumbnails) keep that behaviour instead
/// of failing the whole request later.
///
/// Storage-suffixed paths (`.tgsr`) are uploaded under their logical name
/// (suffix stripped), so Telegram sees the original extension and document
/// names stay clean. Old paths without the suffix keep the previous filename
/// behavior.
async fn path_input(path: &std::path::Path) -> anyhow::Result<InputFile> {
    let logical_name = crate::media::logical_file_name(path);
    let physical_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    tokio::fs::File::open(path).await?;
    let input = InputFile::file(path.to_path_buf());
    Ok(if logical_name.as_str() != physical_name {
        input.file_name(logical_name)
    } else {
        input
    })
}

/// Cache-aware input construction: first look up the file_id cache by
/// "content hash:send mode"; on a hit use `InputFile::Id` directly
/// (Telegram's server already has the file, no re-upload needed).
/// On a miss stream the local file (no whole-file read).
/// The cache key carries a send-mode suffix (photo/gif/video/doc): the same
/// content sent via different modes yields non-interchangeable file_ids, so
/// they are cached separately to avoid cross-use.
async fn resolve_input(
    db: &Db,
    item: &MediaItem,
    mode: &str,
    use_cache: bool,
) -> anyhow::Result<InputFile> {
    if use_cache {
        let key = format!("{}:{}", item.content_hash, mode);
        if let Ok(Some(fid)) = db::get_cached_file_id(db, &key).await {
            return Ok(InputFile::file_id(teloxide::types::FileId(fid)));
        }
    }
    path_input(&item.path).await
}

/// Extract the media file_id from a sent message by send mode (used to write
/// back to the cache).
/// In teloxide 0.17 the media types expose the file_id via `.file.id`
/// (FileId newtype).
fn message_file_id_for(msg: &Message, mode: &str) -> Option<String> {
    match mode {
        "video" => msg.video().map(|v| v.file.id.0.clone()),
        "gif" => msg.animation().map(|a| a.file.id.0.clone()),
        "doc" => msg.document().map(|d| d.file.id.0.clone()),
        _ => msg
            .photo()
            .and_then(|s| s.last())
            .map(|p| p.file.id.0.clone()),
    }
}

/// Send mode of an item inside a media group (consistent with the
/// build_rich_ops / media-group construction logic).
fn group_item_mode(m: &MediaItem) -> &'static str {
    match m.kind {
        MediaKind::Video => "video",
        _ if m.is_gif => "gif",
        _ => "photo",
    }
}

/// Write the file_id back to the cache after a successful send (failures are
/// silently ignored; the push is not affected).
async fn cache_file_id(db: &Db, item: &MediaItem, mode: &str, file_id: Option<String>) {
    if let Some(fid) = file_id {
        let key = format!("{}:{}", item.content_hash, mode);
        db::put_cached_file_id(db, &key, &fid).await.ok();
    }
}

/// Invalidate all cache entries involved by an op (called when Telegram
/// rejects a file_id; afterwards it automatically falls back to re-upload).
async fn invalidate_op_cache(db: &Db, op: &RichOp<'_>) {
    let items: Vec<(&MediaItem, &str)> = match op {
        RichOp::Photo(p) => vec![(p, "photo")],
        RichOp::Gif(g) => vec![(g, "gif")],
        RichOp::Video(v) => vec![(v, "video")],
        RichOp::Group(items) => items.iter().map(|m| (*m, group_item_mode(m))).collect(),
    };
    for (item, mode) in items {
        let key = format!("{}:{}", item.content_hash, mode);
        db::invalidate_cached_file_id(db, &key).await.ok();
    }
}

/// Entry context for one push: collapses the many parameters of
/// send_entry_with_media into a single struct that callers construct once.
/// The fields are destructured into same-named local variables inside the
/// function body; behavior is unchanged.
pub struct PushCtx<'a> {
    pub entry: &'a Entry,
    pub feed_title: &'a str,
    pub author: Option<&'a str>,
    pub opts: RenderOpts,
    pub caption_above_media: bool,
    pub aspect_threshold: f64,
    pub media: &'a [MediaItem],
    /// Structured entry metadata (sidecar); None for RSS feeds /
    /// legacy outbox behavior.
    pub meta: Option<&'a crate::entrymeta::EntryMeta>,
}

/// Split the non-album documents into the ones that can travel inside the
/// rich message and the ones that must follow as separate messages.
///
/// Only image documents (oversized / long / format-converted images) are
/// valid rich attachments. Audio and file attachments stay out: Telegram
/// classifies an uploaded mp3 as audio, and an audio attachment referenced by
/// a `<tg-document>` block is rejected with `RICH_MESSAGE_DOCUMENT_INVALID` —
/// one such item would fail the whole rich message and force the entry back to
/// the standard path. They are sent afterwards as document messages quoting
/// the main message instead.
fn split_rich_docs<'a>(docs: &[&'a MediaItem]) -> (Vec<&'a MediaItem>, Vec<&'a MediaItem>) {
    let mut rich = Vec::new();
    let mut follow = Vec::new();
    for item in docs {
        if item.kind == MediaKind::Image {
            rich.push(*item);
        } else {
            follow.push(*item);
        }
    }
    (rich, follow)
}

/// Classified media for one entry. Retweet inline images are kept separate
/// because they are sent as a follow-up that quotes the main message.
struct MediaPlan<'a> {
    photos: Vec<&'a MediaItem>,
    gifs: Vec<&'a MediaItem>,
    docs: Vec<&'a MediaItem>,
    videos: Vec<&'a MediaItem>,
    rt_photos: Vec<&'a MediaItem>,
}

/// Classify prepared media exactly as the send path needs it.
fn classify_media<'a>(media: &'a [MediaItem], aspect_threshold: f64) -> MediaPlan<'a> {
    let mut photos: Vec<&MediaItem> = Vec::new();
    let mut gifs: Vec<&MediaItem> = Vec::new();
    let mut docs: Vec<&MediaItem> = Vec::new();
    let mut videos: Vec<&MediaItem> = Vec::new();
    let mut rt_photos: Vec<&MediaItem> = Vec::new();
    for m in media {
        // Retweet inline images go through the same oversized/long-image/GIF
        // classification; anything over the limits is merged into docs.
        if m.is_retweet && m.kind == MediaKind::Image {
            let format_doc = !m.is_gif && !crate::media::photo_format_compatible(&m.path);
            if crate::media::image_goes_to_doc(m.size, m.width, m.height, aspect_threshold)
                || format_doc
            {
                docs.push(m);
            } else if m.is_gif {
                gifs.push(m);
            } else {
                rt_photos.push(m);
            }
            continue;
        }
        match m.kind {
            MediaKind::Image => {
                let format_doc = !m.is_gif && !crate::media::photo_format_compatible(&m.path);
                if crate::media::image_goes_to_doc(m.size, m.width, m.height, aspect_threshold)
                    || format_doc
                {
                    docs.push(m);
                    tracing::debug!(
                        size = m.size,
                        w = m.width,
                        h = m.height,
                        format_doc = format_doc,
                        "image -> document (oversized/long/w+h/format)"
                    );
                } else if m.is_gif {
                    // GIFs go through sendAnimation; the photo API does not
                    // play animated images.
                    gifs.push(m);
                } else {
                    photos.push(m);
                }
            }
            MediaKind::Video => videos.push(m),
            MediaKind::Other => docs.push(m),
        }
    }
    MediaPlan {
        photos,
        gifs,
        docs,
        videos,
        rt_photos,
    }
}

struct CaptionPlan {
    caption: String,
    overflow_chunks: Vec<String>,
}

/// Split the full rendered text into the media caption and plain-text overflow.
fn plan_caption(full_caption: &str) -> CaptionPlan {
    let caption_chunks = split_html(full_caption, CAPTION_LIMIT);
    let caption = caption_chunks.first().cloned().unwrap_or_default();
    let overflow_chunks = if caption_chunks.len() > 1 {
        let rest = merge_adjacent_blockquotes(&caption_chunks[1..].join("\n\n"));
        split_html(&rest, SAFE_LIMIT)
    } else {
        Vec::new()
    };
    CaptionPlan {
        caption,
        overflow_chunks,
    }
}

/// Recover after the main rich-media send failed: send the overflow text or
/// the full text fallback, then the documents/retweet images/poll. Returns the
/// final SendOutcome, or Err when nothing was delivered (outbox retry).
#[allow(clippy::too_many_arguments)]
async fn fallback_after_main_media_failure(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    entry: &Entry,
    feed_title: &str,
    author: Option<&str>,
    opts: RenderOpts,
    meta: Option<&crate::entrymeta::EntryMeta>,
    docs: &[&MediaItem],
    rt_photos: &[&MediaItem],
    overflow_chunks: &[String],
    sent: usize,
    delivered: Option<&Message>,
    error: &anyhow::Error,
) -> anyhow::Result<SendOutcome> {
    let mut delivered_tg_link = delivered.and_then(tg_message_link);
    let mut fallback_msg_id: Option<teloxide::types::MessageId> = delivered.map(|m| m.id);
    let mut text_delivered = false;
    let mut text_partial = false;
    if sent > 0 {
        // The first op (which carries the caption) was delivered before the
        // failure; only the caption overflow is sent as follow-up messages.
        text_delivered = true;
        for chunk in overflow_chunks {
            if let Err(te) =
                send_chunk_with_retry(bot, chat_id, chunk, false, fallback_msg_id).await
            {
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    error = %te,
                    "overflow text failed after partial media delivery"
                );
                break;
            }
        }
    } else if let Some((body, footer)) = format_entry_parts(entry, feed_title, author, opts, meta) {
        match send_entry_msg(bot, chat_id, &body, &footer, opts.link_preview).await {
            Ok(sent) => {
                let m = sent.first;
                delivered_tg_link = tg_message_link(&m);
                fallback_msg_id = Some(m.id);
                text_delivered = true;
                text_partial = sent.partial;
            }
            Err(te) => {
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    error = %te,
                    "text fallback also failed, propagating for outbox"
                );
                return Err(anyhow::anyhow!(
                    "media send failed ({}) and text fallback also failed: {}",
                    error,
                    te
                ));
            }
        }
    }
    if !text_delivered {
        return Err(anyhow::anyhow!(
            "media send failed ({}) and no text fallback available",
            error
        ));
    }
    // The text is already delivered; document/rt failures only degrade media.
    for (di, d) in docs.iter().enumerate() {
        if let Err(de) = send_doc_msg(bot, db, chat_id, d, "", fallback_msg_id).await {
            tracing::warn!(
                chat_id = chat_id,
                entry_id = %fetcher::entry_id(entry),
                failed_doc = di,
                skipped_docs = docs.len() - di - 1,
                error = %de,
                "document send failed after text fallback"
            );
            break;
        }
    }
    if !rt_photos.is_empty() {
        let rt_ops = build_rich_ops(rt_photos, &[], &[]);
        if let Err((_, _, re)) = send_rich_ops_with_retry(
            bot,
            db,
            chat_id,
            "",
            false,
            &rt_ops,
            fallback_msg_id,
            "rt_images_text_fallback",
        )
        .await
        {
            tracing::warn!(
                chat_id = chat_id,
                entry_id = %fetcher::entry_id(entry),
                error = %re,
                "retweet images send failed after text fallback"
            );
        }
    }
    try_send_poll(bot, chat_id, fallback_msg_id, meta).await;
    Ok(SendOutcome {
        media_failed: true,
        entry_link: fetcher::entry_link(entry),
        delivered_tg_link,
        delivered_message_id: fallback_msg_id.map(|m| m.0 as i64),
        text_only: sent == 0,
        uncertain: false,
        text_partial,
    })
}

/// Send an entry with prepared media. Strategy:
/// - images ≤10MB (normal aspect) → sendPhoto / sendMediaGroup
/// - images >10MB (photo hard limit) → sendDocument
/// - long images (h/w > threshold) → sendDocument (avoid photo-viewer cropping)
/// - videos → sendVideo (single) or mixed into media group when with images
/// - caption position follows the channel setting for all platforms
/// - transport: standard sends stream the local file as multipart (constant
///   heap memory); rich-message attachments may use file:// on a local Bot API
/// - automatic enhancement: text >1024 or album media >10 may go through one
///   rich message first; any rich failure falls back to the normal path below
/// - any send failure → fall back to text-only so the entry is never lost, and
///   set `media_failed` (reported privately to the chat owner, not the channel).
pub async fn send_entry_with_media(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    ctx: PushCtx<'_>,
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<SendOutcome> {
    let PushCtx {
        entry,
        feed_title,
        author,
        opts,
        caption_above_media,
        aspect_threshold,
        media,
        meta,
    } = ctx;
    // Caption rendering and placement are uniform across platforms; no
    // Weibo-specific handling.
    let full_caption = format_caption(entry, feed_title, author, opts, meta);
    let caption_above = caption_above_media;

    // Title+summary keeps at most one media item. The fetch layer already
    // enforces this for new downloads (prepare_entry_media single_media:
    // stops after the first image/video, never fetches audio or GIFs), so
    // this slice is a compatibility guard for outbox rows serialized by
    // older versions: when images are present only the first one is sent
    // (no full album group); otherwise the existing video/document behavior
    // is kept.
    // (Title+link / title-only modes already skip media downloads at the
    // scheduler layer and never reach here.)
    let media: &[MediaItem] = if opts.mode == ContentMode::TitleSummary {
        match media.iter().position(|m| m.kind == MediaKind::Image) {
            Some(i) => &media[i..i + 1],
            None => media,
        }
    } else {
        media
    };

    if media.is_empty() {
        // No media: plain text message path (split into multiple if needed,
        // footer placed on the last message). The first message link is
        // returned so download warnings can link back to the delivered entry.
        return send_text_only(bot, chat_id, entry, feed_title, author, opts, meta).await;
    }

    let CaptionPlan {
        caption,
        overflow_chunks,
    } = plan_caption(&full_caption);

    let MediaPlan {
        photos,
        gifs,
        docs,
        videos,
        rt_photos,
    } = classify_media(media, aspect_threshold);

    // Op-list send: each op == one Telegram API call, so a retried send can
    // resume after the last successful op without duplicating messages.
    // Rich media (images/GIFs/videos) are sent first as the entry's main
    // message; the converted-to-document messages then follow as "quote
    // replies" to the main message, making it easy to jump between them.
    let rich_ops = build_rich_ops(&photos, &gifs, &videos);
    // Network errors (proxy blips / upstream hiccups) get bounded exponential
    // backoff, resuming from the last successful op each time so partial
    // progress is never re-sent. Non-network errors (bad request, parse) fail
    // fast to the text fallback.
    // The entry's main message (the first rich-media message); document
    // messages quote it for easy navigation.
    let mut anchor: Option<Message> = None;
    // True once the caption carrier (first rich op or the full-text fallback)
    // has been delivered; the post-send sanity check below must not treat a
    // partial media failure as "caption undelivered" and trigger an outbox
    // replay that duplicates it.
    let mut caption_delivered = false;
    let mut media_failed = false;
    let mut rich_delivered = false;

    // Automatic enhancement: only posts that would overflow the existing
    // caption/album limits take the rich path. The count covers exactly the
    // media that enter the rich message (album + image documents); audio/file
    // attachments are follow-up messages and neither trigger nor widen the
    // rich payload (see split_rich_docs). GIFs and retweet images also keep
    // their existing follow-up messages.
    let album_media: Vec<&MediaItem> = videos
        .iter()
        .copied()
        .chain(photos.iter().copied())
        .collect();
    let (rich_docs, follow_docs) = split_rich_docs(&docs);
    let rich_media_count = album_media.len() + rich_docs.len();
    let full_text_len = visible_len(&full_caption);
    tracing::debug!(
        chat_id = chat_id,
        entry_id = %fetcher::entry_id(entry),
        text_len = full_text_len,
        media_count = rich_media_count,
        document_count = rich_docs.len(),
        follow_up_documents = follow_docs.len(),
        gif_count = gifs.len(),
        retweet_images = rt_photos.len(),
        allow_rich = opts.allow_rich,
        rich_eligible = opts.allow_rich
            && !crate::richmsg::is_unsupported()
            && should_try_rich_media(full_text_len, rich_media_count),
        "/push: media path decision"
    );
    if opts.allow_rich
        && !crate::richmsg::is_unsupported()
        && should_try_rich_media(full_text_len, rich_media_count)
    {
        match try_send_rich_media(
            bot,
            db,
            chat_id,
            &full_caption,
            caption_above,
            &album_media,
            &rich_docs,
            transport,
        )
        .await
        {
            Ok(m) => {
                anchor = Some(m);
                caption_delivered = true;
                rich_delivered = true;
                tracing::info!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    text_len = full_text_len,
                    media_count = rich_media_count,
                    document_count = rich_docs.len(),
                    follow_up_documents = follow_docs.len(),
                    "rich media enhancement sent"
                );
            }
            Err(e) => {
                let err_str = error_chain_string(e.as_ref());
                crate::richmsg::note_send_error(&err_str);
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    text_len = full_text_len,
                    media_count = rich_media_count,
                    document_count = rich_docs.len(),
                    follow_up_documents = follow_docs.len(),
                    error = %err_str,
                    "rich media send failed"
                );
                // A1 policy: do not enter the standard media path (which would
                // resend the full text + every media item, duplicating a
                // delivery that may already have happened). Ambiguous
                // outcomes propagate as `uncertain`.
                if crate::sendpolicy::is_ambiguous_send_error(&err_str) {
                    tracing::warn!(
                        chat_id = chat_id,
                        entry_id = %fetcher::entry_id(entry),
                        "rich media outcome uncertain; not falling back to standard path"
                    );
                    return Ok(SendOutcome {
                        entry_link: fetcher::entry_link(entry),
                        uncertain: true,
                        ..SendOutcome::default()
                    });
                }
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    "rich media failure is explicitly undelivered, falling back to standard path"
                );
            }
        }
    }

    // GIFs are deliberately outside the rich payload (and its trigger count).
    // When the rich main message was sent, keep the historical standalone
    // animation follow-ups, quoting it.
    if rich_delivered && !gifs.is_empty() {
        let gif_ops = build_rich_ops(&[], &gifs, &[]);
        let reply_to = anchor.as_ref().map(|m| m.id);
        if let Err((_, _, e)) =
            send_rich_ops_with_retry(bot, db, chat_id, "", false, &gif_ops, reply_to, "rich_gifs")
                .await
        {
            tracing::warn!(
                chat_id = chat_id,
                entry_id = %fetcher::entry_id(entry),
                error = %e,
                "gif follow-up after rich message failed"
            );
            media_failed = true;
        }
    }

    if !rich_delivered && !rich_ops.is_empty() {
        let rich = send_rich_ops_with_retry(
            bot,
            db,
            chat_id,
            &caption,
            caption_above,
            &rich_ops,
            None,
            "main",
        )
        .await;
        match rich {
            Ok((_, first_msg)) => {
                anchor = first_msg;
                caption_delivered = true;
            }
            Err((sent, delivered, e)) => {
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    failed_op = sent,
                    total_ops = rich_ops.len(),
                    error = %e,
                    "media send failed, falling back to text"
                );
                return fallback_after_main_media_failure(
                    bot,
                    db,
                    chat_id,
                    entry,
                    feed_title,
                    author,
                    opts,
                    meta,
                    &docs,
                    &rt_photos,
                    &overflow_chunks,
                    sent,
                    delivered.as_deref(),
                    &e,
                )
                .await;
            }
        }
    }

    // Converted-to-document messages: multiple documents are sent as a group
    // via sendMediaGroup (InputMediaDocument), appearing visually as one
    // unit, which naturally solves the "scattered into N independent,
    // unrelated messages" problem (previously, with one-by-one sendDocument
    // and no rich-media main message, anchor was None and the document
    // messages had no quote at all). On group failure it falls back to
    // one-by-one sending as a safety net so nothing is lost.
    // When the entry consists only of documents (no rich media), the caption
    // is placed on the first document of the first group.
    // A document send failure does not fall back to plain text (the body was
    // already delivered with the main message); it only sets media_failed
    // and relies on the admin notification as the safety net.
    let mut first_doc_msg: Option<Message> = None;
    // Documents already traveled inside the rich message; only the follow-up
    // audio/file attachments are sent here, quoting it. A rich failure leaves
    // rich_delivered=false and keeps the normal document-group path with every
    // document in its original order.
    let docs_to_send: &[&MediaItem] = if rich_delivered { &follow_docs } else { &docs };
    if !docs_to_send.is_empty() {
        let reply_to = anchor.as_ref().map(|m| m.id);
        let cap = if anchor.is_none() {
            caption.as_str()
        } else {
            ""
        };
        match send_doc_group(bot, db, chat_id, docs_to_send, cap, reply_to).await {
            Ok(msgs) => {
                first_doc_msg = msgs.into_iter().next();
            }
            Err((delivered, e)) => {
                tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %e, delivered = delivered.len(), total_docs = docs_to_send.len(), "doc group send failed, falling back to one-by-one");
                // Fall back to one-by-one sending: skip the part already
                // delivered as a group (in multi-group scenarios, when group
                // 2 fails, group 1 is already delivered and must not be
                // re-sent). When there is no rich-media main message (anchor
                // is None), subsequent documents quote the first document,
                // avoiding a recurrence of the "scattered, unrelated" defect.
                let mut chain_id: Option<teloxide::types::MessageId> = reply_to;
                // First message of the already-delivered part: used as
                // first_doc_msg and as the quote anchor for what follows
                if let Some(first) = delivered.first() {
                    first_doc_msg = Some(first.clone());
                    if anchor.is_none() && chain_id.is_none() {
                        chain_id = Some(first.id);
                    }
                }
                for (i, d) in docs_to_send.iter().enumerate().skip(delivered.len()) {
                    let cap = if i == 0 && anchor.is_none() {
                        caption.as_str()
                    } else {
                        ""
                    };
                    match send_doc_msg(bot, db, chat_id, d, cap, chain_id).await {
                        Ok(m) => {
                            if first_doc_msg.is_none() {
                                first_doc_msg = Some(m.clone());
                            }
                            // Without an anchor, the first document becomes
                            // the quote target for subsequent documents
                            if anchor.is_none() && chain_id.is_none() {
                                chain_id = Some(m.id);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %e, "document send failed");
                            media_failed = true;
                            break;
                        }
                    }
                }
            }
        }
    }

    // Inline images from the retweeted original post (decoded from Weibo
    // [view image] links): merged into a single media message, quoting the
    // main message (anchor) for easy navigation.
    // Special case: when the entry's only media are all retweet images (the
    // main post has no images) → rich_ops is empty and docs is empty, so the
    // caption (body text) has no other carrier; in that case attach it to
    // the first rt op to prevent losing the body text.
    if !rt_photos.is_empty() {
        let rt_is_sole_carrier = anchor.is_none() && first_doc_msg.is_none();
        let rt_caption = if rt_is_sole_carrier {
            caption.as_str()
        } else {
            ""
        };
        let rt_reply_to = anchor.as_ref().or(first_doc_msg.as_ref()).map(|m| m.id);
        let rt_ops = build_rich_ops(&rt_photos, &[], &[]);
        // Network backoff retry consistent with the main rich_ops (resume
        // from the breakpoint).
        let rt_res = send_rich_ops_with_retry(
            bot,
            db,
            chat_id,
            rt_caption,
            caption_above,
            &rt_ops,
            rt_reply_to,
            "rt_images",
        )
        .await;
        match rt_res {
            Ok((_, first_rt_msg)) => {
                // When retweet images become the only delivered carrier,
                // record the anchor so subsequent overflow can quote it.
                if rt_is_sole_carrier {
                    if let Some(m) = first_rt_msg {
                        anchor = Some(m);
                    }
                }
            }
            Err((sent, delivered, e)) => {
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    failed_op = sent,
                    total_ops = rt_ops.len(),
                    error = %e,
                    "retweet inline images send failed"
                );
                media_failed = true;
                // A non-zero `sent` means op 0 completed. When these rt
                // images are the sole caption carrier, the caption was
                // delivered even though a later group failed; marking it
                // undelivered here would enqueue the entry for outbox retry
                // and re-send the already delivered group every backoff
                // until the 72h abandon. Keep the delivered message so
                // overflow/poll can quote it and /find records a usable id.
                if rt_is_sole_carrier && sent > 0 {
                    caption_delivered = true;
                    if anchor.is_none() {
                        anchor = delivered.map(|m| *m);
                    }
                }
            }
        }
    }

    // Caption overflow follows the entry's messages. For document-only
    // entries the first document carries the caption, and the overflow text
    // follows right after it. Overflow chunks uniformly quote the first
    // message (the anchor rich-media main message or the first document),
    // flat structure, consistent with plain-text chunking.
    // Note: if rich media/documents are already delivered (anchor or
    // first_doc_msg is set), an overflow text failure does not return Err —
    // otherwise an upstream retry would re-send the media as a whole and
    // create duplicates. Instead, set media_failed and continue returning Ok.
    let overflow_reply_to: Option<teloxide::types::MessageId> =
        anchor.as_ref().or(first_doc_msg.as_ref()).map(|m| m.id);
    // `caption_delivered` also covers a partial rt/rich send whose message
    // object is unavailable: an overflow failure must not trigger a whole
    // entry replay and duplicate the caption.
    let already_delivered = caption_delivered || anchor.is_some() || first_doc_msg.is_some();
    // The rich message carries the entire text, so the standard caption
    // overflow must not be sent again.
    if !rich_delivered {
        for chunk in &overflow_chunks {
            if let Err(e) =
                send_chunk_with_retry(bot, chat_id, chunk, false, overflow_reply_to).await
            {
                if already_delivered {
                    tracing::warn!(chat_id = chat_id, entry_id = %fetcher::entry_id(entry), error = %e, "overflow text failed after delivery, not retrying");
                    media_failed = true;
                    break;
                }
                return Err(e);
            }
        }
    }

    // The first caption chunk is attached to the first
    // carrier (document/retweet images). If all carriers fail (both anchor
    // and first_doc_msg are None), the first 1024 characters of the body
    // were never delivered — even if the overflow text succeeded, the entry
    // is still incomplete.
    // The previous guard additionally required !overflow_delivered, which
    // caused "all carriers failed + overflow succeeded" to wrongly return
    // Ok, the entry to be marked sent, and the first caption chunk to be
    // permanently lost.
    // Fix: as long as all caption carriers fail, return Err for outbox retry
    // regardless of whether the overflow was delivered. The cost is that the
    // overflow text may be duplicated on retry, but that is better than
    // losing the body text.
    if !caption_delivered
        && anchor.is_none()
        && first_doc_msg.is_none()
        && (!docs.is_empty() || !rt_photos.is_empty())
    {
        return Err(anyhow::anyhow!(
            "all caption carriers (documents/retweet images) failed to send; caption undelivered"
        ));
    }

    // Weibo poll: after body completeness is established, convert the poll
    // data into a native Telegram poll. Sending it before the carrier check
    // meant "all documents failed -> return Err" retried the poll on every
    // outbox backoff (polls cannot be recalled).
    let poll_reply_to = anchor.as_ref().or(first_doc_msg.as_ref()).map(|m| m.id);
    if try_send_poll(bot, chat_id, poll_reply_to, meta).await {
        media_failed = true;
    }

    let delivered_tg_link = anchor
        .as_ref()
        .and_then(tg_message_link)
        .or_else(|| first_doc_msg.as_ref().and_then(tg_message_link));
    let delivered_message_id = anchor
        .as_ref()
        .or(first_doc_msg.as_ref())
        .map(|m| m.id.0 as i64);

    Ok(SendOutcome {
        media_failed,
        entry_link: fetcher::entry_link(entry),
        delivered_tg_link,
        delivered_message_id,
        text_only: false,
        uncertain: false,
        text_partial: false,
    })
}

/// One media send operation (granularity = one Telegram API call).
/// Converted-to-document messages do not go through here — they are sent
/// separately after the rich-media main message and quote it.
enum RichOp<'a> {
    Photo(&'a MediaItem),
    Gif(&'a MediaItem),
    Video(&'a MediaItem),
    Group(Vec<&'a MediaItem>),
}

/// Order rich media (videos/GIFs/images) as "photo/video album first → GIFs
/// last", splitting the album pool into groups of ≤10 each.
/// Telegram sendMediaGroup allows at most 10 media per call; anything beyond
/// that is split into follow-up albums, which quote the first album's
/// message when sent for easy navigation.
/// Photos and videos are merged into one pool (videos leading, images after)
/// because Telegram's sendMediaGroup accepts them in the same album — the
/// shared type constraint only applies to documents and audios (which must be
/// grouped with same-type peers), so those still go through `send_doc_group`
/// as follow-up messages that quote the album.
/// GIFs must be separated into independent ops (sendAnimation) and always
/// follow the album: Telegram's sendMediaGroup does not accept the animation
/// type — mixing one into an image/video album gets the whole album rejected
/// (Bad Request: type "animation" can't be used in sendMediaGroup).
fn build_rich_ops<'a>(
    photos: &[&'a MediaItem],
    gifs: &[&'a MediaItem],
    videos: &[&'a MediaItem],
) -> Vec<RichOp<'a>> {
    let mut ops: Vec<RichOp<'a>> = Vec::new();

    // Videos + images share one album pool (videos first, preserving the
    // historical "video leads" ordering for multi-media entries).
    let mut pool: Vec<&'a MediaItem> = Vec::with_capacity(videos.len() + photos.len());
    pool.extend_from_slice(videos);
    pool.extend_from_slice(photos);

    // ALBUM_LIMIT never yields an empty slice, so len is always ≥1.
    // A single item cannot form an album (sendMediaGroup requires 2–10), so
    // dispatch it to the dedicated single-media op by kind.
    for chunk in pool.chunks(ALBUM_LIMIT) {
        if chunk.len() == 1 {
            if chunk[0].kind == MediaKind::Video {
                ops.push(RichOp::Video(chunk[0]));
            } else {
                ops.push(RichOp::Photo(chunk[0]));
            }
        } else {
            ops.push(RichOp::Group(chunk.to_vec()));
        }
    }

    // GIFs are sent one by one independently (sendAnimation), never in an
    // album, and always after the album (they cannot share the album anyway).
    for g in gifs {
        ops.push(RichOp::Gif(g));
    }

    ops
}

/// Send converted-to-document media as a group (sendMediaGroup +
/// InputMediaDocument).
/// Groups of ≤10 documents each; the first group carries the caption (if
/// any), and follow-up groups quote the first message of the first group.
/// Uses the file_id cache (mode="doc"); a rejected file_id is automatically
/// invalidated and re-uploaded with one retry.
/// Returns all sent messages (in order); the caller takes the first one as
/// the anchor/link.
async fn send_doc_group(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    docs: &[&MediaItem],
    caption: &str,
    reply_to: Option<teloxide::types::MessageId>,
) -> Result<Vec<Message>, (Vec<Message>, anyhow::Error)> {
    let chat = ChatId(chat_id);
    let caption = normalize_blockquote_fold(caption);
    let caption = caption.as_str();

    // sendMediaGroup requires 2-10 media items; a single document goes
    // through sendDocument (consistent with build_rich_ops sending a single
    // photo via send_photo).
    if docs.len() == 1 {
        let m = send_doc_msg(bot, db, chat_id, docs[0], caption, reply_to)
            .await
            .map_err(|e| (Vec::new(), e))?;
        return Ok(vec![m]);
    }

    let mut all_msgs: Vec<Message> = Vec::new();
    let mut first_msg_id: Option<teloxide::types::MessageId> = None;

    for (gi, chunk) in docs.chunks(ALBUM_LIMIT).enumerate() {
        // The caption goes only on the first document of the first group;
        // follow-up groups carry no caption
        let group_caption = if gi == 0 { caption } else { "" };
        // Follow-up groups quote the first message of the first group
        let reply_msg_id = if gi > 0 { first_msg_id } else { reply_to };
        let rp = reply_msg_id.map(teloxide::types::ReplyParameters::new);

        // sendMediaGroup requires 2-10 items; a trailing single document
        // (11/21/31 docs) must go through sendDocument instead of a group.
        // send_doc_msg does its own pacing, so do not pace twice here.
        if chunk.len() == 1 {
            let m = send_doc_msg(bot, db, chat_id, chunk[0], group_caption, reply_msg_id)
                .await
                .map_err(|e| (all_msgs.clone(), e))?;
            if first_msg_id.is_none() {
                first_msg_id = Some(m.id);
            }
            all_msgs.push(m);
            continue;
        }

        pace(chat_id).await;
        let keys: Vec<String> = chunk
            .iter()
            .map(|d| format!("{}:doc", d.content_hash))
            .collect();
        let has_thumb = chunk.iter().any(|d| d.thumb_path.is_some());
        let msgs = send_with_full_retry(db, chat_id, &keys, has_thumb, |use_cache, use_thumbs| {
            let rp = rp.clone();
            async move {
                let mut group: Vec<InputMedia> = Vec::new();
                for (j, d) in chunk.iter().enumerate() {
                    let inp = resolve_input(db, d, "doc", use_cache).await?;
                    let mut im = InputMediaDocument::new(inp);
                    // Image documents opt out of content-type detection: a WebP
                    // document is otherwise detected as a sticker, and stickers
                    // cannot carry a caption. With detection disabled the message
                    // stays a document and keeps its caption (verified on both
                    // the official and local Bot API).
                    if d.kind == MediaKind::Image {
                        im.disable_content_type_detection = Some(true);
                    }
                    if j == 0 && !group_caption.is_empty() {
                        im = im
                            .caption(group_caption.to_string())
                            .parse_mode(ParseMode::Html);
                    }
                    // Converted-to-document long/large images carry a thumbnail
                    if use_thumbs {
                        if let Some(tp) = &d.thumb_path {
                            if let Ok(tin) = path_input(tp).await {
                                im.thumbnail = Some(tin);
                            }
                        }
                    }
                    group.push(InputMedia::Document(im));
                }
                let mut req = bot.send_media_group(chat, group);
                if let Some(ref params) = rp {
                    req = req.reply_parameters(params.clone());
                }
                req.await.map_err(|e| anyhow::anyhow!(e))
            }
        })
        .await
        .map_err(|e| (all_msgs.clone(), e))?;
        // Write cache: returned messages correspond to the chunk in order
        for (m, d) in msgs.iter().zip(chunk.iter()) {
            cache_file_id(db, d, "doc", message_file_id_for(m, "doc")).await;
        }
        if first_msg_id.is_none() {
            first_msg_id = msgs.first().map(|m| m.id);
        }
        all_msgs.extend(msgs);
    }
    Ok(all_msgs)
}

/// Retry skeleton shared by document sends: file_id cache invalidation,
/// thumbnail fallback, flood control, and transient backoff.
///
/// `keys` holds one file_id cache key per item (a media group has several).
/// `has_thumb` enables the "drop thumbnail and retry" branch; the attempt
/// closure receives `(use_cache, use_thumb)`.
async fn send_with_full_retry<T, F, Fut>(
    db: &Db,
    chat_id: i64,
    keys: &[String],
    has_thumb: bool,
    mut attempt: F,
) -> anyhow::Result<T>
where
    F: FnMut(bool, bool) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut retry = SendRetryState::new(SendRetryPolicy::DOCUMENT, true, has_thumb);
    loop {
        match attempt(retry.use_file_id(), retry.use_thumbnail()).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let es = error_chain_string(e.as_ref());
                match retry.next_action(&es) {
                    SendRetryAction::DropFileIdCache => {
                        for key in keys {
                            db::invalidate_cached_file_id(db, key).await.ok();
                        }
                        pace(chat_id).await;
                        continue;
                    }
                    SendRetryAction::DropThumbnail => {
                        tracing::warn!(chat_id = chat_id, error = %es, "thumbnail rejected, retrying without it");
                        continue;
                    }
                    SendRetryAction::RetryFlood { delay } => {
                        tracing::warn!(chat_id = chat_id, wait_secs = delay.as_secs(), error = %es, "send flood control, backing off");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    SendRetryAction::RetryTransient { delay } => {
                        tracing::warn!(chat_id = chat_id, backoff = delay.as_secs(), error = %es, "send transient error, backing off");
                        tokio::time::sleep(delay).await;
                        pace(chat_id).await;
                        continue;
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}

/// Send a single converted-to-document message (sendDocument). The document
/// message quotes the entry's main message (reply_to) so it is easy to jump
/// between the document and the body text; when the entry consists only of
/// documents, the caption is placed on the first document. Also uses the
/// file_id cache (mode="doc").
async fn send_doc_msg(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    d: &MediaItem,
    caption: &str,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    pace(chat_id).await;
    // Unified blockquote folding rules
    let caption = normalize_blockquote_fold(caption);
    let caption = caption.as_str();
    // file_id cache retry: the first round uses the cached file_id
    // (use_cache=true); if Telegram rejects it (file_id expired), invalidate
    // that cache entry and re-upload once from the local file.
    // resolve_input(use_cache=false) is equivalent to calling path_input
    // directly, matching the behavior of the original retry path.
    let key = format!("{}:doc", d.content_hash);
    let m = send_with_full_retry(
        db,
        chat_id,
        std::slice::from_ref(&key),
        d.thumb_path.is_some(),
        |use_cache, attach_thumb| async move {
            let inp = resolve_input(db, d, "doc", use_cache).await?;
            let mut req = bot.send_document(ChatId(chat_id), inp);
            // Image documents opt out of content-type detection: a WebP document
            // is otherwise detected as a sticker, and stickers cannot carry a
            // caption (the title/summary text would be silently lost). Verified on
            // both the official and local Bot API: with detection disabled the
            // message stays a document and keeps its caption.
            if d.kind == MediaKind::Image {
                req = req.disable_content_type_detection(true);
            }
            // Converted-to-document long/large images carry a thumbnail so the
            // message list shows a preview; a thumbnail-related 400 drops it and
            // retries so the document itself is still delivered.
            if attach_thumb {
                if let Some(tp) = &d.thumb_path {
                    if let Ok(tin) = path_input(tp).await {
                        req = req.thumbnail(tin);
                    }
                }
            }
            if !caption.is_empty() {
                // Note: Telegram's show_caption_above_media only supports
                // photo/video/animation/paid media; a document message's caption
                // is always fixed below the file.
                req = req.caption(caption).parse_mode(ParseMode::Html);
            }
            if let Some(rid) = reply_to {
                req = req.reply_parameters(teloxide::types::ReplyParameters::new(rid));
            }
            req.await.map_err(|e| anyhow::anyhow!(e))
        },
    )
    .await?;
    cache_file_id(db, d, "doc", message_file_id_for(&m, "doc")).await;
    Ok(m)
}

/// Failure of a rich-op send: (failed op index, first delivered message,
/// error). The message is boxed because teloxide's `Message` is large and
/// clippy rejects a ~2KB Err variant; it is needed to keep the caption
/// carrier's link/id after a partial send across retries.
type RichSendFailure = (usize, Option<Box<Message>>, anyhow::Error);

/// Execute ops starting at `resume`. Returns total op count on success, or
/// (ops completed, error) on failure so the caller can resume.
/// On success returns the first sent message (for document messages to quote
/// and for generating message links).
/// Each op internally uses the file_id cache (a hit avoids the upload; a
/// rejected file_id is automatically invalidated and re-uploaded).
/// With multiple groups: the caption goes only on the first group, and
/// follow-up groups quote the first group's message.
// The 8 parameters all share the same push context; packing them into yet
// another struct would be over-engineering.
#[allow(clippy::too_many_arguments)]
async fn send_rich_ops(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    caption: &str,
    caption_above: bool,
    ops: &[RichOp<'_>],
    resume: usize,
    // When set, the FIRST op replies to this message (used by retweet
    // inline images to quote the main entry message).
    reply_first_to: Option<teloxide::types::MessageId>,
    // On a resumed call, the already-delivered first message (from a previous
    // attempt) so later ops keep the original quote anchor instead of losing
    // it when `first_msg` starts empty.
    resume_anchor: Option<teloxide::types::MessageId>,
) -> Result<(usize, Option<Message>), RichSendFailure> {
    let mut first_msg: Option<Message> = None;
    for (i, op) in ops.iter().enumerate().skip(resume) {
        // Queue centrally per channel to keep parallel sources from hitting
        // flood control
        pace(chat_id).await;
        // Caption goes only on the first group; follow-up groups quote the
        // first group's message
        let op_caption = if i == 0 { caption } else { "" };
        let reply_to = if i > 0 {
            first_msg.as_ref().map(|m| m.id).or(resume_anchor)
        } else {
            reply_first_to
        };
        match send_op(bot, db, chat_id, op_caption, caption_above, op, reply_to).await {
            Err(e) => return Err((i, first_msg.map(Box::new), anyhow::anyhow!("{:#}", e))),
            Ok(msgs) => {
                if first_msg.is_none() {
                    first_msg = msgs.into_iter().next();
                }
            }
        }
    }
    Ok((ops.len(), first_msg))
}

/// send_rich_ops + bounded backoff with resume on network errors. The main
/// rich_ops and rt_photos sites previously duplicated the same loop
/// verbatim; only connection-class errors that are "definitely not
/// delivered" are retried, resuming after the last successful op; already
/// delivered parts are never re-sent.
#[allow(clippy::too_many_arguments)]
async fn send_rich_ops_with_retry(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    caption: &str,
    caption_above: bool,
    ops: &[RichOp<'_>],
    reply_first_to: Option<teloxide::types::MessageId>,
    ctx: &str,
) -> Result<(usize, Option<Message>), RichSendFailure> {
    let mut first_delivered: Option<Box<Message>> = None;
    let mut res = send_rich_ops(
        bot,
        db,
        chat_id,
        caption,
        caption_above,
        ops,
        0,
        reply_first_to,
        None,
    )
    .await;
    if let Err((_, first, _)) = &res {
        first_delivered = first.clone();
    }
    let mut retry = SendRetryState::new(SendRetryPolicy::TRANSIENT_ONLY, false, false);
    loop {
        let retry_from = match &res {
            Err((sent, _, e)) if *sent < ops.len() => {
                let es = error_chain_string(e.as_ref());
                match retry.next_action(&es) {
                    SendRetryAction::RetryTransient { delay } => Some((*sent, delay)),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((sent, delay)) = retry_from else {
            break;
        };
        tracing::warn!(
            chat_id = chat_id,
            ctx = ctx,
            resume = sent,
            backoff = delay.as_secs(),
            "rich send network error, backing off before retry"
        );
        tokio::time::sleep(delay).await;
        let resume_anchor = first_delivered.as_ref().map(|m| m.id);
        res = send_rich_ops(
            bot,
            db,
            chat_id,
            caption,
            caption_above,
            ops,
            sent,
            reply_first_to,
            resume_anchor,
        )
        .await;
        if first_delivered.is_none() {
            if let Err((_, first, _)) = &res {
                first_delivered = first.clone();
            }
        }
    }
    match res {
        // Keep the message from the first attempt: a resumed successful call
        // only returns the first message of the resumed slice, not the true
        // caption/first-op message delivered before the failure.
        Ok((total, first)) => Ok((total, first_delivered.map(|m| *m).or(first))),
        Err((sent, first, e)) => Err((sent, first_delivered.or(first), e)),
    }
}

/// Send a single op (granularity = one Telegram API call), with built-in
/// file_id cache:
/// on a cache hit use `InputFile::Id` to avoid re-uploading; if Telegram
/// rejects that file_id (bot changed / file cleaned up), invalidate the
/// cache entry and retry once with a fresh upload.
/// Returns all messages produced by the op (multiple for media groups) so
/// the caller can take the first one as the anchor.
/// When `reply_to` is set, the message quotes the specified message
/// (follow-up groups quote the first group).
///
/// The file_id cache retry + flood backoff skeleton was
/// extracted into send_with_cache_retry; each media-type branch only
/// provides a closure that "builds and sends the request given use_cache".
#[allow(clippy::too_many_arguments)]
async fn send_op(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    caption: &str,
    caption_above: bool,
    op: &RichOp<'_>,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Vec<Message>> {
    let chat = ChatId(chat_id);
    // Unified blockquote folding rules (caption is shared across all media
    // types)
    let caption_owned = normalize_blockquote_fold(caption);
    let caption = caption_owned.as_str();
    let reply_params = reply_to.map(teloxide::types::ReplyParameters::new);
    // async move closures require all captures to be Copy/references;
    // reply_params is lifted to a reference.
    let rp = &reply_params;

    match op {
        RichOp::Photo(p) => {
            let m =
                send_with_cache_retry(db, chat_id, op, false, |cache_enabled, _attach_thumb| {
                    let use_cache = cache_enabled;
                    async move {
                        let inp = resolve_input(db, p, "photo", use_cache).await?;
                        let mut req = bot
                            .send_photo(chat, inp)
                            .caption(caption)
                            .parse_mode(ParseMode::Html)
                            .show_caption_above_media(caption_above);
                        if let Some(rp) = rp {
                            req = req.reply_parameters(rp.clone());
                        }
                        req.await.map_err(|e| anyhow::anyhow!(e))
                    }
                })
                .await?;
            cache_file_id(db, p, "photo", message_file_id_for(&m, "photo")).await;
            Ok(vec![m])
        }
        RichOp::Gif(g) => {
            let m =
                send_with_cache_retry(db, chat_id, op, false, |cache_enabled, _attach_thumb| {
                    let use_cache = cache_enabled;
                    async move {
                        let inp = resolve_input(db, g, "gif", use_cache).await?;
                        let mut req = bot
                            .send_animation(chat, inp)
                            .caption(caption)
                            .parse_mode(ParseMode::Html)
                            .show_caption_above_media(caption_above);
                        if let Some(rp) = rp {
                            req = req.reply_parameters(rp.clone());
                        }
                        req.await.map_err(|e| anyhow::anyhow!(e))
                    }
                })
                .await?;
            cache_file_id(db, g, "gif", message_file_id_for(&m, "gif")).await;
            Ok(vec![m])
        }
        RichOp::Video(v) => {
            let m = send_with_cache_retry(
                db,
                chat_id,
                op,
                v.thumb_path.is_some(),
                |cache_enabled, attach_thumb| {
                    let use_cache = cache_enabled;
                    async move {
                        let inp = resolve_input(db, v, "video", use_cache).await?;
                        let mut req = bot
                            .send_video(chat, inp)
                            .caption(caption)
                            .parse_mode(ParseMode::Html)
                            .show_caption_above_media(caption_above)
                            .supports_streaming(true);
                        if let Some(d) = v.duration_secs {
                            req = req.duration(d);
                        }
                        if attach_thumb {
                            if let Some(tp) = &v.thumb_path {
                                if let Ok(tin) = path_input(tp).await {
                                    req = req.thumbnail(tin);
                                }
                            }
                        }
                        if let Some(rp) = rp {
                            req = req.reply_parameters(rp.clone());
                        }
                        req.await.map_err(|e| anyhow::anyhow!(e))
                    }
                },
            )
            .await?;
            cache_file_id(db, v, "video", message_file_id_for(&m, "video")).await;
            Ok(vec![m])
        }
        RichOp::Group(items) => {
            let has_thumb = items.iter().any(|m| m.thumb_path.is_some());
            let msgs =
                send_with_cache_retry(db, chat_id, op, has_thumb, |cache_enabled, attach_thumb| {
                    let use_cache = cache_enabled;
                    async move {
                        let group = build_group_inputs(
                            db,
                            items,
                            caption,
                            caption_above,
                            use_cache,
                            attach_thumb,
                        )
                        .await?;
                        let mut req = bot.send_media_group(chat, group);
                        if let Some(rp) = rp {
                            req = req.reply_parameters(rp.clone());
                        }
                        req.await.map_err(|e| anyhow::anyhow!(e))
                    }
                })
                .await?;
            // Write cache: returned messages correspond to items in order
            for (m, item) in msgs.iter().zip(items.iter()) {
                let mode = group_item_mode(item);
                cache_file_id(db, item, mode, message_file_id_for(m, mode)).await;
            }
            Ok(msgs)
        }
    }
}

/// Generic skeleton for file_id cache retry + flood backoff.
/// `attempt(use_cache)` builds and sends the request; when a file_id is
/// rejected the cache is invalidated and it retries once with caching off;
/// on flood 429 it backs off as Telegram requires, then retries.
async fn send_with_cache_retry<T, F, Fut>(
    db: &Db,
    chat_id: i64,
    op: &RichOp<'_>,
    has_thumb: bool,
    mut attempt: F,
) -> anyhow::Result<T>
where
    F: FnMut(bool, bool) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut retry = SendRetryState::new(SendRetryPolicy::RICH_OP, true, has_thumb);
    loop {
        match attempt(retry.use_file_id(), retry.use_thumbnail()).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let es = error_chain_string(e.as_ref());
                match retry.next_action(&es) {
                    SendRetryAction::DropThumbnail => {
                        tracing::warn!(chat_id = chat_id, error = %es, "thumbnail rejected, retrying without it");
                        continue;
                    }
                    SendRetryAction::DropFileIdCache => {
                        invalidate_op_cache(db, op).await;
                        pace(chat_id).await;
                        continue;
                    }
                    SendRetryAction::RetryFlood { delay } => {
                        tracing::warn!(chat_id = chat_id, wait_secs = delay.as_secs(), error = %es, "op send flood control, backing off");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}

/// Build the InputMedia list for a media group. When `use_cache=true` the
/// cached file_id is preferred.
async fn build_group_inputs(
    db: &Db,
    items: &[&MediaItem],
    caption: &str,
    caption_above: bool,
    use_cache: bool,
    use_thumb: bool,
) -> anyhow::Result<Vec<InputMedia>> {
    let mut group: Vec<InputMedia> = Vec::new();
    for (j, m) in items.iter().enumerate() {
        let mode = group_item_mode(m);
        let inp = resolve_input(db, m, mode, use_cache).await?;
        match m.kind {
            MediaKind::Video => {
                let mut im = InputMediaVideo::new(inp);
                im.supports_streaming = Some(true);
                if j == 0 {
                    im = im.caption(caption.to_string()).parse_mode(ParseMode::Html);
                }
                // Telegram requires show_caption_above_media to be
                // the same for ALL items in a media group.
                im.show_caption_above_media = caption_above;
                // Same as send_video: explicitly attach thumbnail/duration
                // so the preview is shown
                if let Some(d) = m.duration_secs {
                    im.duration = Some(d.min(65535) as u16);
                }
                if use_thumb {
                    if let Some(tp) = &m.thumb_path {
                        if let Ok(tin) = path_input(tp).await {
                            im.thumbnail = Some(tin);
                        }
                    }
                }
                group.push(InputMedia::Video(im));
            }
            // Note: GIFs are already separated into independent ops
            // (sendAnimation) in build_rich_ops and never enter a media
            // group — Telegram's sendMediaGroup does not accept the
            // animation type. Here everything is treated uniformly as a
            // photo as a defensive fallback.
            _ => {
                let mut im = InputMediaPhoto::new(inp);
                if j == 0 {
                    im = im.caption(caption.to_string()).parse_mode(ParseMode::Html);
                }
                im.show_caption_above_media = caption_above;
                group.push(InputMedia::Photo(im));
            }
        }
    }
    Ok(group)
}

/// Result of sending one entry's text chunks.
pub struct TextSendOutcome {
    /// The first delivered message (anchor / /find registration).
    pub first: Message,
    /// True when the first chunk was delivered but a later chunk failed. The
    /// caller must not queue the entry for an outbox retry (that would replay
    /// the delivered prefix); it marks the entry sent and warns the owner.
    pub partial: bool,
}

/// Send an entry and return the FIRST sent message (used to build the
/// Telegram message link for owner failure notices).
/// When split into multiple messages, each subsequent message replies to the
/// previous one for easy navigation. The footer is placed on the last message.
///
/// Chunk-level retry is bounded within this call. If a later chunk still
/// fails after the first chunk was delivered, the function returns
/// `partial = true` instead of an error: a whole-entry outbox retry would
/// re-send the already delivered chunks every backoff until the 72h abandon.
pub async fn send_entry_msg(
    bot: &Bot,
    chat_id: i64,
    html_text: &str,
    footer: &str,
    link_preview: bool,
) -> anyhow::Result<TextSendOutcome> {
    let (footer, chunks) = plan_text_chunks(html_text, footer);
    if chunks.is_empty() {
        anyhow::bail!("refusing to send an empty text message");
    }
    let mut first_msg: Option<Message> = None;
    let last_idx = chunks.len() - 1;
    for (i, chunk) in chunks.iter().enumerate() {
        // Append footer to the last chunk only.
        let text = if i == last_idx && !footer.is_empty() {
            format!("{}\n\n{}", chunk, footer)
        } else {
            chunk.clone()
        };
        // Only the first chunk gets link preview (avoid duplicate previews).
        let lp = if i == 0 { link_preview } else { false };
        // Chunks uniformly quote the first message (flat structure, not
        // chained).
        let reply_to = first_msg.as_ref().map(|m| m.id);
        // Chunk-level bounded backoff retry: on failure only the current
        // chunk is re-sent.
        match send_chunk_with_retry(bot, chat_id, &text, lp, reply_to).await {
            Ok(m) => {
                if first_msg.is_none() {
                    first_msg = Some(m);
                }
            }
            Err(e) => {
                if let Some(first) = first_msg {
                    tracing::warn!(
                        chat_id = chat_id,
                        failed_chunk = i,
                        total_chunks = chunks.len(),
                        error = %e,
                        "text chunk failed after an earlier chunk was delivered; not retrying the whole entry"
                    );
                    return Ok(TextSendOutcome {
                        first,
                        partial: true,
                    });
                }
                return Err(e);
            }
        }
        // Pacing between chunks is handled by send_single_html's pace().
    }
    Ok(TextSendOutcome {
        first: first_msg.expect("at least one chunk"),
        partial: false,
    })
}

/// Send a single HTML message (must already fit within Telegram's limit).
/// Handles flood-control retry and HTML parse fallback.
/// If `reply_to` is set, the message replies to that message id.
async fn send_single_html(
    bot: &Bot,
    chat_id: i64,
    html_text: &str,
    link_preview: bool,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    // Unified blockquote folding rules (fold when >200 chars).
    let html_text = normalize_blockquote_fold(html_text);
    let lpo = if link_preview {
        None
    } else {
        Some(no_preview())
    };
    let mut retry = SendRetryState::new(SendRetryPolicy::TEXT, false, false);
    let mut plain_text: Option<String> = None;

    loop {
        // Queue centrally per channel (including flood retries and the
        // plain-text fallback) so parallel sources don't hit rate limits.
        pace(chat_id).await;
        let text = plain_text.as_deref().unwrap_or(html_text.as_str());
        let mut req = bot.send_message(ChatId(chat_id), text);
        if plain_text.is_none() {
            req = req.parse_mode(ParseMode::Html);
        }
        if let Some(o) = &lpo {
            req = req.link_preview_options(o.clone());
        }
        if let Some(rid) = reply_to {
            req = req.reply_parameters(teloxide::types::ReplyParameters::new(rid));
        }

        match req.await {
            Ok(m) => return Ok(m),
            Err(e) => {
                let err_str = error_chain_string(&e);
                match retry.next_action(&err_str) {
                    SendRetryAction::RetryFlood { delay } => {
                        tracing::warn!(
                            chat_id = chat_id,
                            wait_secs = delay.as_secs(),
                            "flood control, backing off"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    SendRetryAction::FallbackPlainText => {
                        // Stripping all formatting is a user-visible
                        // degradation; leave a trace so we can track down
                        // which HTML triggered it.
                        tracing::warn!(
                            chat_id = chat_id,
                            error = %err_str,
                            html_head = %html_text.chars().take(100).collect::<String>(),
                            "HTML parse failed, falling back to plain text"
                        );
                        plain_text = Some(crate::htmltext::strip_html_keep_links(&html_text));
                        continue;
                    }
                    _ => return Err(e.into()),
                }
            }
        }
    }
}

/// Send one already-split HTML chunk with bounded network/transient-TG
/// backoff. Flood-control retry lives inside send_single_html; this layer
/// covers the connection-class retry shared by the text-chunk and
/// caption-overflow paths (the latter previously had no retry at all).
async fn send_chunk_with_retry(
    bot: &Bot,
    chat_id: i64,
    text: &str,
    link_preview: bool,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    let mut retry = SendRetryState::new(SendRetryPolicy::TRANSIENT_ONLY, false, false);
    loop {
        match send_single_html(bot, chat_id, text, link_preview, reply_to).await {
            Ok(m) => return Ok(m),
            Err(e) => {
                let es = error_chain_string(e.as_ref());
                match retry.next_action(&es) {
                    SendRetryAction::RetryTransient { delay } => {
                        tracing::warn!(
                            chat_id = chat_id,
                            backoff = delay.as_secs(),
                            "text chunk send failed, retrying"
                        );
                        tokio::time::sleep(delay).await;
                        pace(chat_id).await;
                        continue;
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}

/// Build a t.me link for a message in a channel/supergroup (best effort).
pub fn tg_message_link(msg: &Message) -> Option<String> {
    let cid = msg.chat.id.0;
    // Channels / supergroups have a public t.me/c/<id>/<msg_id> link using the
    // id without the -100 prefix. Negative IDs without the -100 prefix
    // (regular group chats etc.) have no public link.
    let bare = if cid < 0 {
        let s = cid.to_string();
        let x = s.strip_prefix("-100")?;
        x.to_string()
    } else {
        return None;
    };
    Some(format!("https://t.me/c/{}/{}", bare, msg.id.0))
}

// Escape HTML special characters / attribute escaping / chat-id & title
// formatting / plain-text stripping now live in `crate::htmltext`
// (re-exported above for external callers).

#[cfg(test)]
mod tests {
    use super::render::*;
    use super::*;
    use crate::htmltext::truncate_html_keep_links;

    // ===== extract_poll =====

    /// Build an EntryMeta containing a poll (simulating what weibo.rs
    /// produces).
    fn meta_with_poll(
        question: &str,
        options: Vec<&str>,
        is_multi: bool,
    ) -> crate::entrymeta::EntryMeta {
        crate::entrymeta::EntryMeta {
            prerendered_html: false,
            poll: Some(crate::entrymeta::PollData {
                question: question.to_string(),
                options: options.into_iter().map(|s| s.to_string()).collect(),
                is_multi_select: is_multi,
            }),
            retweet_images: Vec::new(),
        }
    }

    #[test]
    fn extract_poll_basic() {
        let meta = meta_with_poll("你搬过家吗？", vec!["A：没搬过", "B：搬过"], false);
        let (q, opts, multi) = extract_poll(Some(&meta)).unwrap();
        assert_eq!(q, "你搬过家吗？");
        assert_eq!(opts, vec!["A：没搬过", "B：搬过"]);
        assert!(!multi);
    }

    #[test]
    fn extract_poll_multi_select() {
        let meta = meta_with_poll("多选", vec!["甲", "乙", "丙"], true);
        let (_, opts, multi) = extract_poll(Some(&meta)).unwrap();
        assert!(multi);
        assert_eq!(opts.len(), 3);
    }

    #[test]
    fn extract_poll_truncates_long_question_and_options() {
        // question >300 chars is truncated, option >100 chars is truncated
        let long_q = "问".repeat(310);
        let long_opt = "选".repeat(110);
        let meta = meta_with_poll(&long_q, vec![&long_opt, "短"], false);
        let (q, opts, _) = extract_poll(Some(&meta)).unwrap();
        assert!(
            q.chars().count() <= 300,
            "question too long: {}",
            q.chars().count()
        );
        assert!(q.ends_with("..."));
        assert!(
            opts[0].chars().count() <= 100,
            "option too long: {}",
            opts[0].chars().count()
        );
        assert!(opts[0].ends_with("..."));
    }

    #[test]
    fn extract_poll_caps_at_10_options() {
        // 12 options → only the first 10 are kept (Telegram's cap)
        let opts: Vec<&str> = (0..12).map(|_| "选项").collect();
        let meta = meta_with_poll("多选", opts, false);
        let (_, options, _) = extract_poll(Some(&meta)).unwrap();
        assert_eq!(options.len(), 10);
    }

    #[test]
    fn extract_poll_rejects_single_option() {
        // Only 1 valid option → does not meet Telegram's minimum of 2 items
        let meta = meta_with_poll("问题", vec!["唯一"], false);
        assert!(extract_poll(Some(&meta)).is_none());
    }

    #[test]
    fn extract_poll_no_meta() {
        // No meta (RSS feed) → no poll
        assert!(extract_poll(None).is_none());
    }

    #[test]
    fn extract_poll_meta_without_poll() {
        // meta exists but has no poll field
        let meta = crate::entrymeta::EntryMeta::default();
        assert!(extract_poll(Some(&meta)).is_none());
    }

    // ===== entry_summary hyperlink handling =====

    fn rss_entry_with_body(body: &str) -> Entry {
        crate::platforms::make_entry(
            "test-id".to_string(),
            "测试标题".to_string(),
            String::new(),
            body.to_string(),
            "text/html",
            "https://example.com/post".to_string(),
            None,
        )
    }

    #[test]
    fn entry_summary_keeps_wechat_account_link_clickable() {
        // huxiu (RSSHub) real shape: the WeChat account name sits inside an
        // <a> right after "本文来自微信公众号：". The summary must keep both
        // the anchor text and the hyperlink (the account name alone was the
        // previous fix; the href was dropped along with it).
        let body = "<p>本文来自微信公众号：<a href=\"https://mp.weixin.qq.com/s?x=1&amp;y=2\"> 示例公众号 </a>，作者：张三 李四</p><p>正文开始，第二段内容。</p>";
        let summary = entry_summary(&rss_entry_with_body(body), 140, None);
        assert!(
            summary.contains("<a href=\"https://mp.weixin.qq.com/s?x=1&amp;y=2\">示例公众号</a>"),
            "hyperlink lost: {summary}"
        );
        assert!(
            summary.contains("本文来自微信公众号："),
            "lead lost: {summary}"
        );
        // The account name stays in its sentence position (anchor text must
        // not drift away from the surrounding words).
        assert!(
            summary.contains("微信公众号：<a href=\"https://mp.weixin.qq.com/s?x=1&amp;y=2\">示例公众号</a>，作者"),
            "anchor misplaced: {summary}"
        );
        assert!(summary.contains("正文开始"), "body lost: {summary}");
    }

    #[test]
    fn entry_summary_skips_inline_style_source() {
        // Bear Blog shape (blog.solazy.me): two short paragraphs, then an
        // inline dark-mode <style> block before the image group. The CSS used
        // to start around the 79th visible character, so it filled the latter
        // part of the 140-char summary as apparent source code.
        let body = concat!(
            "<p>这周又来长沙出差了。新的工作应该会经常来长沙。巧的是，长沙确实是我非常喜欢的城市。</p>\n",
            "<p>周一上午落地后第一顿就吃了衡东菜，虽说一人食，但也是点了三个菜。</p>\n",
            "<style>\n",
            "    @media (prefers-color-scheme: dark) {\n",
            "        .#img-group-hesq8 .desc-text { color: rgba(255, 255, 255, 0.7) !important; }\n",
            "    }\n",
            "    .dark-preview-mode .#img-group-hesq8 .desc-text { color: rgba(255, 255, 255, 0.7) !important; }\n",
            "</style>\n",
            "<div id=\"img-group-hesq8\"><img src=\"https://example.com/a.jpg\"><p>▲ 图片说明</p></div>",
        );
        let summary = entry_summary(&rss_entry_with_body(body), 140, None);
        assert!(
            !summary.contains("@media"),
            "css leaked into summary: {summary}"
        );
        assert!(
            !summary.contains("prefers-color-scheme"),
            "css leaked into summary: {summary}"
        );
        assert!(
            !summary.contains("rgba("),
            "css leaked into summary: {summary}"
        );
        assert!(
            summary.contains("这周又来长沙出差了"),
            "lead lost: {summary}"
        );
        assert!(
            summary.contains("衡东菜"),
            "second paragraph lost: {summary}"
        );
    }

    #[test]
    fn entry_summary_closes_anchor_when_cut_inside_link() {
        // Truncation inside the anchor text must re-close <a> so the summary
        // stays valid Telegram HTML, with the ellipsis outside the link.
        let long_label = "长".repeat(60);
        let body = format!(
            "<p>前文。<a href=\"https://mp.weixin.qq.com/s/abc\">{long_label}</a>后续内容。</p>"
        );
        let summary = entry_summary(&rss_entry_with_body(&body), 20, None);
        assert!(
            summary.ends_with("</a>......"),
            "unbalanced or misplaced suffix: {summary}"
        );
        assert_eq!(
            summary.matches("<a ").count(),
            1,
            "unbalanced anchor: {summary}"
        );
        assert_eq!(
            summary.matches("</a>").count(),
            1,
            "unbalanced anchor: {summary}"
        );
        assert!(summary.starts_with("前文。"), "lead lost: {summary}");
    }

    #[test]
    fn entry_summary_drops_anchor_without_visible_text_in_budget() {
        // An <a> whose text starts past the cut point must not appear as an
        // empty hyperlink.
        let body = "<p>正文内容比较长按 here</p><p><a href=\"https://mp.weixin.qq.com/s/x\">后面的链接</a></p>";
        let summary = entry_summary(&rss_entry_with_body(body), 6, None);
        assert!(
            !summary.contains("<a"),
            "empty/dangling anchor emitted: {summary}"
        );
        assert!(!summary.contains("</a>"), "orphan close emitted: {summary}");
        assert!(
            summary.ends_with("......"),
            "truncation marker missing: {summary}"
        );
    }

    #[test]
    fn entry_summary_never_splits_entity_reference() {
        // Text entities survive the body pipeline as references. The cut
        // counts each reference as one display character and must emit it
        // whole: a split "&am" makes Telegram reject the message with a
        // parse-entities error.
        let body = "<p>成本&lt;100&amp;更多&gt;一些文字继续更多内容截断部分。</p>";
        // Budget ends exactly after the "&amp;" reference.
        let summary = entry_summary(&rss_entry_with_body(body), 7, None);
        assert_eq!(
            summary, "成本&lt;100&amp;......",
            "entity cut wrong: {summary}"
        );
        // Budget ends just before it: nothing of the reference may appear.
        let summary = entry_summary(&rss_entry_with_body(body), 6, None);
        assert_eq!(summary, "成本&lt;100......", "entity cut wrong: {summary}");
    }

    #[test]
    fn entry_summary_keeps_video_anchor() {
        // Video links survive the body pipeline, so the summary keeps the
        // clickable card instead of dropping the anchor and its label.
        let body = "<p>正文。</p><a href=\"https://video.weibo.com/show?fid=1034:1\">微博视频</a>";
        let summary = entry_summary(&rss_entry_with_body(body), 140, None);
        assert!(summary.contains("正文。"), "body lost: {summary}");
        assert!(
            summary.contains(r#"<a href="https://video.weibo.com/show?fid=1034:1">微博视频</a>"#),
            "video anchor lost: {summary}"
        );
    }

    #[test]
    fn truncate_html_input_respects_utf8_and_tag_boundaries() {
        let html = "<p>第一个段落。</p><p>第二个段落。</p>";
        let cut = truncate_html_input(html, 25);
        assert!(html.starts_with(cut));
        assert!(cut.ends_with('>'), "cut inside a tag: {cut:?}");

        // Cap lands inside a multi-byte character: back off to a boundary.
        let text = "中文".repeat(10);
        let cut = truncate_html_input(&text, 5);
        assert_eq!(cut, "中");

        // A malformed tag fragment with no closing '>' yields an empty prefix
        // so the caller can fall back to the full conversion.
        assert_eq!(truncate_html_input("<img src=\"unfinished", 8), "");
    }

    #[test]
    fn entry_summary_large_body_uses_prefix_without_changing_output() {
        // A body far above the summary input cap must produce exactly the
        // same excerpt as the old full-body conversion.
        let body = format!("<p>{}</p>", "正文".repeat(200_000));
        let entry = rss_entry_with_body(&body);
        let full = fetcher::entry_body_html(&entry, 100_000, None);
        let (cut, truncated) = truncate_html_keep_links(&full, SUMMARY_MAX_CHARS);
        let expected = if truncated {
            format!("{}......", cut.trim())
        } else {
            cut.trim().to_string()
        };
        assert_eq!(entry_summary(&entry, SUMMARY_MAX_CHARS, None), expected);
    }

    #[test]
    fn entry_summary_prefix_all_markup_falls_back_to_full_body() {
        // The first SUMMARY_INPUT_CAP bytes are only <img> markup; the prefix
        // has no visible text, so the summary must fall back to the full body.
        let body = format!(
            "{}<p>后面的正文</p>",
            "<img src=\"https://example.com/x\">".repeat(20_000)
        );
        let summary = entry_summary(&rss_entry_with_body(&body), 140, None);
        assert_eq!(summary, "后面的正文");
    }

    // ===== build_rich_ops (album pool: videos + images in one album) =====

    /// Minimal MediaItem factory for op-assembly tests: only `kind`/`is_gif`
    /// drive build_rich_ops, and `content_hash` labels each item so tests can
    /// assert the resulting order.
    fn mk_item(hash: &str, kind: MediaKind, is_gif: bool) -> MediaItem {
        MediaItem {
            path: std::path::PathBuf::from(format!("{hash}.bin")),
            kind,
            width: 0,
            height: 0,
            size: 0,
            is_gif,
            thumb_path: None,
            duration_secs: None,
            content_hash: hash.to_string(),
            is_retweet: false,
        }
    }

    /// Flatten the ops into an ordered `(label, group?)` list for assertions:
    /// each op becomes one entry, and album-group members are joined by "+"
    /// so their internal order is checked too.
    fn op_labels(ops: &[RichOp<'_>]) -> Vec<String> {
        ops.iter()
            .map(|op| match op {
                RichOp::Photo(p) => format!("photo:{}", p.content_hash),
                RichOp::Gif(g) => format!("gif:{}", g.content_hash),
                RichOp::Video(v) => format!("video:{}", v.content_hash),
                RichOp::Group(items) => {
                    let parts: Vec<String> =
                        items.iter().map(|m| m.content_hash.to_string()).collect();
                    format!("group[{}]", parts.join("+"))
                }
            })
            .collect()
    }

    #[test]
    fn rich_ops_merge_one_video_one_image_into_single_album() {
        // The core of this change: a video and an image must land in the SAME
        // first album (video leading), not "video msg + image quoting it".
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let ops = build_rich_ops(&[&p], &[], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1]"]);
    }

    #[test]
    fn rich_ops_video_leads_image_within_merged_album() {
        // Ordering contract: all videos precede all images inside the pool.
        let v = mk_item("v1", MediaKind::Video, false);
        let p1 = mk_item("p1", MediaKind::Image, false);
        let p2 = mk_item("p2", MediaKind::Image, false);
        let ops = build_rich_ops(&[&p1, &p2], &[], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1+p2]"]);
    }

    #[test]
    fn rich_ops_single_item_falls_back_to_direct_send() {
        // sendMediaGroup requires 2–10 items, so a lone media goes through
        // its dedicated single op, dispatched by kind.
        let v = mk_item("v1", MediaKind::Video, false);
        assert_eq!(
            op_labels(&build_rich_ops(&[], &[], &[&v])),
            vec!["video:v1"]
        );
        let p = mk_item("p1", MediaKind::Image, false);
        assert_eq!(
            op_labels(&build_rich_ops(&[&p], &[], &[])),
            vec!["photo:p1"]
        );
    }

    #[test]
    fn rich_ops_gifs_are_standalone_and_follow_album() {
        // GIFs cannot enter an album and must be emitted after it.
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let g1 = mk_item("g1", MediaKind::Image, true);
        let g2 = mk_item("g2", MediaKind::Image, true);
        let ops = build_rich_ops(&[&p], &[&g1, &g2], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1]", "gif:g1", "gif:g2"]);
    }

    #[test]
    fn rich_ops_overflow_splits_into_follow_up_albums() {
        // 12 media > the 10-per-album cap: first album fills to 10, the
        // remainder becomes a follow-up album (which the sender replies to
        // the first). Videos lead, then images fill the same pool.
        let videos: Vec<MediaItem> = (0..3)
            .map(|i| mk_item(&format!("v{i}"), MediaKind::Video, false))
            .collect();
        let photos: Vec<MediaItem> = (0..9)
            .map(|i| mk_item(&format!("p{i}"), MediaKind::Image, false))
            .collect();
        let vrefs: Vec<&MediaItem> = videos.iter().collect();
        let prefs: Vec<&MediaItem> = photos.iter().collect();
        let ops = build_rich_ops(&prefs, &[], &vrefs);
        assert_eq!(ops.len(), 2);
        assert_eq!(
            op_labels(&ops),
            vec!["group[v0+v1+v2+p0+p1+p2+p3+p4+p5+p6]", "group[p7+p8]",]
        );
    }

    #[test]
    fn rich_ops_title_summary_slice_still_yields_single_photo() {
        // Compatibility guard: TitleSummary keeps only the first image, so
        // the pool is a lone image and must not become an album.
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let media = [v.clone(), p.clone()];
        let sliced: &[MediaItem] = match media.iter().position(|m| m.kind == MediaKind::Image) {
            Some(i) => &media[i..i + 1],
            None => &media,
        };
        let prefs: Vec<&MediaItem> = sliced
            .iter()
            .filter(|m| m.kind == MediaKind::Image && !m.is_gif)
            .collect();
        let vrefs: Vec<&MediaItem> = sliced
            .iter()
            .filter(|m| m.kind == MediaKind::Video)
            .collect();
        let ops = build_rich_ops(&prefs, &[], &vrefs);
        assert_eq!(op_labels(&ops), vec!["photo:p1"]);
    }

    #[test]
    fn rich_text_trigger_uses_strict_standard_limit() {
        assert!(!should_try_rich_text(SAFE_LIMIT));
        assert!(should_try_rich_text(SAFE_LIMIT + 1));
        assert!(should_try_rich_text(RICH_TEXT_MAX));
        assert!(!should_try_rich_text(RICH_TEXT_MAX + 1));
    }

    #[test]
    fn text_only_boundary_reserves_the_two_char_separator() {
        // send_entry_msg reserves footer+2 even with no footer, so its
        // effective body budget is 4094. A 4095-char body is split by the
        // standard path and must trigger the rich enhancement; 4094 is the
        // last single-message body.
        let body_4094 = "x".repeat(SAFE_LIMIT - 2);
        let body_4095 = "x".repeat(SAFE_LIMIT - 1);
        assert_eq!(standard_text_len(&body_4094, ""), SAFE_LIMIT);
        assert!(!should_try_rich_text(standard_text_len(&body_4094, "")));
        assert_eq!(standard_text_len(&body_4095, ""), SAFE_LIMIT + 1);
        assert!(should_try_rich_text(standard_text_len(&body_4095, "")));

        // With a footer, the same reservation applies to the combined text.
        // "via x" is 5 visible chars; 4096 - 5 - 2 = 4089 fits exactly.
        assert_eq!(
            standard_text_len(&"x".repeat(SAFE_LIMIT - 7), "via x"),
            SAFE_LIMIT
        );
        assert!(!should_try_rich_text(standard_text_len(
            &"x".repeat(SAFE_LIMIT - 7),
            "via x"
        )));
        assert!(should_try_rich_text(standard_text_len(
            &"x".repeat(SAFE_LIMIT - 6),
            "via x"
        )));
    }

    #[test]
    fn rich_media_trigger_counts_album_and_document_attachments() {
        // ≤10 attachments with a normal caption stay standard.
        assert!(!should_try_rich_media(CAPTION_LIMIT, 10));
        assert!(should_try_rich_media(CAPTION_LIMIT + 1, 10));
        // >10 total attachments trigger, regardless of type (documents are
        // counted the same as photos/videos).
        assert!(should_try_rich_media(0, 11));
        // A single document + overflowing text triggers the rich path.
        assert!(should_try_rich_media(CAPTION_LIMIT + 1, 1));
        // No media (GIF/document excluded upstream or empty): do not trigger.
        assert!(!should_try_rich_media(CAPTION_LIMIT + 1, 0));
        // Rich hard limits.
        assert!(!should_try_rich_media(
            CAPTION_LIMIT + 1,
            RICH_MEDIA_MAX + 1
        ));
        assert!(!should_try_rich_media(RICH_TEXT_MAX + 1, 11));
    }

    #[test]
    fn rich_html_respects_caption_above_media() {
        let media = "<tg-collage><img src=\"tg://photo?id=m0\"/></tg-collage>";
        assert_eq!(
            rich_html("body", "footer", media, true),
            format!("body<br><br>footer{media}")
        );
        assert_eq!(
            rich_html("body", "footer", media, false),
            format!("{media}body<br><br>footer")
        );
        assert_eq!(rich_html("body", "", "", true), "body");
    }

    #[test]
    fn rich_html_preserves_blockquote_linebreaks_and_pre_verbatim() {
        let body = "<blockquote>line1\nline2</blockquote>\n\n<pre>code1\ncode2</pre>";
        assert_eq!(
            rich_html(body, "", "", true),
            "<blockquote>line1<br>line2</blockquote><br><pre>code1\ncode2</pre>"
        );
    }

    #[test]
    fn rich_html_matches_standard_line_counts_around_blockquote() {
        // Main text -> retweet blockquote is one newline in the standard path;
        // blockquote -> footer is two. The block boundary supplies one line
        // break, so only the second transition needs one extra <br>.
        let body = "main\n<blockquote>rt1\nrt2</blockquote>";
        assert_eq!(
            rich_html(body, "via source", "", true),
            "main<blockquote>rt1<br>rt2</blockquote><br>via source"
        );
    }

    #[test]
    fn rich_html_converts_crlf_outside_pre() {
        let body = "a\r\nb<pre>c\r\nd</pre>";
        assert_eq!(rich_html(body, "", "", true), "a<br>b<pre>c\r\nd</pre>");
    }

    #[test]
    fn rich_media_html_uses_single_collage_and_slideshow() {
        let one = mk_item("p0", MediaKind::Image, false);
        assert_eq!(
            rich_media_html(&[&one], &[]),
            "<img src=\"tg://photo?id=m0\"/>"
        );

        let two = mk_item("p1", MediaKind::Image, false);
        assert_eq!(
            rich_media_html(&[&one, &two], &[]),
            "<tg-collage><img src=\"tg://photo?id=m0\"/><img src=\"tg://photo?id=m1\"/></tg-collage>"
        );

        let items: Vec<MediaItem> = (0..11)
            .map(|i| mk_item(&format!("p{i}"), MediaKind::Image, false))
            .collect();
        let refs: Vec<&MediaItem> = items.iter().collect();
        let html = rich_media_html(&refs, &[]);
        assert!(html.starts_with("<tg-slideshow>"));
        assert!(html.ends_with("</tg-slideshow>"));
    }

    #[test]
    fn rich_media_html_appends_document_blocks_after_album() {
        let photo = mk_item("p0", MediaKind::Image, false);
        let doc_image = mk_item("d0", MediaKind::Image, false);
        let doc_image2 = mk_item("d1", MediaKind::Image, false);

        assert_eq!(
            rich_media_html(&[&photo], &[&doc_image, &doc_image2]),
            "<img src=\"tg://photo?id=m0\"/>\
             <tg-document src=\"tg://document?id=m1\"></tg-document>\
             <tg-document src=\"tg://document?id=m2\"></tg-document>"
        );

        // Documents-only entries start their ids at m0.
        assert_eq!(
            rich_media_html(&[], &[&doc_image, &doc_image2]),
            "<tg-document src=\"tg://document?id=m0\"></tg-document>\
             <tg-document src=\"tg://document?id=m1\"></tg-document>"
        );
    }

    #[test]
    fn split_rich_docs_keeps_audio_and_files_out_of_the_rich_payload() {
        // Only image documents are valid rich attachments. Audio/file
        // attachments must follow as separate messages: an mp3 referenced as
        // a <tg-document> block is rejected by Telegram
        // (RICH_MESSAGE_DOCUMENT_INVALID) and would drop the whole rich
        // message back to the standard path.
        let doc_image = mk_item("d0", MediaKind::Image, false);
        let audio = mk_item("d1", MediaKind::Other, false);
        let file = mk_item("d2", MediaKind::Other, false);

        let (rich, follow) = split_rich_docs(&[&doc_image, &audio, &file]);

        assert_eq!(rich.len(), 1);
        assert_eq!(rich[0].content_hash, "d0");
        assert_eq!(follow.len(), 2);
        assert_eq!(follow[0].content_hash, "d1");
        assert_eq!(follow[1].content_hash, "d2");
    }

    #[test]
    fn rich_media_html_never_groups_documents_into_slideshow() {
        // Collages/slideshows only accept photos and videos; >10 documents
        // must stay individual blocks.
        let docs: Vec<MediaItem> = (0..11)
            .map(|i| mk_item(&format!("d{i}"), MediaKind::Image, false))
            .collect();
        let refs: Vec<&MediaItem> = docs.iter().collect();
        let html = rich_media_html(&[], &refs);
        assert_eq!(html.matches("<tg-document").count(), 11);
        assert!(!html.contains("<tg-slideshow>"));
        assert!(!html.contains("<tg-collage>"));
    }

    #[test]
    fn content_mode_accepts_legacy_camel_case_defaults() {
        assert_eq!(
            ContentMode::from_str("TitleSummary"),
            ContentMode::TitleSummary
        );
        assert_eq!(
            ContentMode::from_str("TitleContent"),
            ContentMode::TitleContent
        );
        assert_eq!(
            ContentMode::from_str("content_only"),
            ContentMode::ContentOnly
        );
        assert_eq!(ContentMode::from_str("MediaOnly"), ContentMode::MediaOnly);
        assert_eq!(ContentMode::from_str("unknown"), ContentMode::TitleLink);
    }

    // ===== footer / split planning =====

    #[test]
    fn plan_text_chunks_keeps_last_chunk_plus_footer_within_limit() {
        let body = "正文".repeat(500);
        let footer = format!("<i>via {}</i>", "长".repeat(4090));
        let (footer, chunks) = plan_text_chunks(&body, &footer);

        assert!(visible_len(&footer) <= SAFE_LIMIT - 2 - MIN_TEXT_CHUNK);
        assert!(chunks.iter().all(|c| visible_len(c) <= MIN_TEXT_CHUNK));
        let last = format!("{}\n\n{}", chunks.last().unwrap(), footer);
        assert!(visible_len(&last) <= SAFE_LIMIT);
    }

    #[test]
    fn plan_text_chunks_entity_heavy_footer_does_not_degrade_to_one_char() {
        let body = "x".repeat(100);
        let footer = format!("<i>via {}</i>", "&amp;".repeat(820));
        let (footer, chunks) = plan_text_chunks(&body, &footer);

        assert!(visible_len(&footer) <= SAFE_LIMIT - 2 - MIN_TEXT_CHUNK);
        assert_eq!(chunks.len(), 1);
        assert_eq!(visible_len(&chunks[0]), 100);
        let last = format!("{}\n\n{}", chunks[0], footer);
        assert!(visible_len(&last) <= SAFE_LIMIT);
    }

    #[test]
    fn plan_text_chunks_empty_body_allows_long_footer_but_fits() {
        let footer = format!("<i>via {}</i>", "y".repeat(4090));
        let (footer, chunks) = plan_text_chunks("", &footer);

        assert_eq!(chunks.len(), 1);
        let last = format!("\n\n{}", footer);
        assert!(visible_len(&last) <= SAFE_LIMIT);
    }

    #[test]
    fn plan_text_chunks_keeps_4094_body_in_one_message() {
        let body = "x".repeat(SAFE_LIMIT - 2);
        let (footer, chunks) = plan_text_chunks(&body, "");

        assert!(footer.is_empty());
        assert_eq!(chunks.len(), 1);
        assert_eq!(visible_len(&chunks[0]), SAFE_LIMIT - 2);
    }

    #[test]
    fn source_line_caps_display_title_and_author() {
        let entry = rss_entry_with_body("");
        let long = "标".repeat(300);

        let line = source_line(&entry, &long, None, SourceDisplay::Source);
        assert_eq!(visible_len(&line), 4 + SOURCE_DISPLAY_MAX);

        let line = source_line(&entry, "t", Some(&long), SourceDisplay::Author);
        assert_eq!(visible_len(&line), 8 + SOURCE_DISPLAY_MAX);
    }
}
