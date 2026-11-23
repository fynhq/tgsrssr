//! Entry sending: text/media split, rich messages, retries and fallbacks.
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

pub use crate::htmltext::{
    escape_attr, escape_html, format_chat_id_html, format_chat_title_html,
    normalize_blockquote_fold, take_utf16_prefix, utf16_len, visible_len,
};

mod render;
pub use render::{channel_header_text, channel_sub_counts};
use render::{
    format_caption, format_caption_with_mode, format_entry_parts, format_entry_parts_with_mode,
    plan_text_chunks, rich_html, rich_media_html, should_try_rich_media, should_try_rich_text,
    standard_text_len,
};

const SAFE_LIMIT: usize = 4096;
// Leave headroom below SAFE_LIMIT for markup growth and the reply-chain overhead.
pub(crate) const CHUNK_LIMIT: usize = 4000;
const CAPTION_LIMIT: usize = 1024;
const RICH_TEXT_MAX: usize = 32768;
const RICH_MEDIA_MAX: usize = 50;
const RICH_MULTIPART_MEDIA_MAX: u64 = 128 * 1024 * 1024;
const ALBUM_LIMIT: usize = 10;

pub fn no_preview() -> teloxide::types::LinkPreviewOptions {
    teloxide::types::LinkPreviewOptions {
        is_disabled: true,
        url: None,
        prefer_small_media: false,
        prefer_large_media: false,
        show_above_text: false,
    }
}

pub async fn retire_prompt(
    bot: &Bot,
    chat: teloxide::types::ChatId,
    prompt_msg_id: i32,
    status: &str,
) {
    let mid = teloxide::types::MessageId(prompt_msg_id);
    if bot.edit_message_text(chat, mid, status).await.is_err() {
        tracing::warn!(
            chat_id = chat.0,
            message_id = prompt_msg_id,
            "retire_prompt: edit failed, leaving message in place (no delete)"
        );
    }
}

#[derive(Debug, Clone, Default)]
pub struct SendOutcome {
    pub media_failed: bool,
    pub entry_link: Option<String>,
    pub delivered_tg_link: Option<String>,
    pub delivered_message_id: Option<i64>,
    pub text_only: bool,
    // A1: an ambiguous or partially delivered send must never be replayed; mark sent and notify the owner.
    pub uncertain: bool,
    pub text_partial: bool,
}

pub fn extract_poll(
    meta: Option<&crate::entrymeta::EntryMeta>,
) -> Option<(String, Vec<String>, bool)> {
    let poll = meta.and_then(|m| m.poll.as_ref())?;
    truncate_poll(&poll.question, &poll.options, poll.is_multi_select)
}

fn truncate_poll(
    question: &str,
    options: &[String],
    is_multi: bool,
) -> Option<(String, Vec<String>, bool)> {
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
    if let Err(e) = bot.pin_chat_message(ChatId(chat_id), msg.id).await {
        tracing::warn!(chat_id = chat_id, error = %e, "pin poll message failed (missing permission?)");
    }
    Ok(msg)
}

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
        let (rich_body, rich_footer) = format_entry_parts_with_mode(
            entry,
            feed_title,
            author,
            opts,
            meta,
            fetcher::TableMode::Rich,
        )
        .unwrap_or_else(|| (body.clone(), footer.clone()));
        match try_send_rich_text(bot, chat_id, &rich_body, &rich_footer).await {
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContentMode {
    TitleLink,
    TitleSummary,
    TitleContent,
    TitleOnly,
    ContentOnly,
    MediaOnly,
}

impl ContentMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "title_summary" | "titlesummary" => Self::TitleSummary,
            "title_content" | "titlecontent" => Self::TitleContent,
            "title_only" | "titleonly" => Self::TitleOnly,
            "content_only" | "contentonly" => Self::ContentOnly,
            "media_only" | "mediaonly" => Self::MediaOnly,
            _ => Self::TitleLink,
        }
    }

    pub fn skip_media(&self) -> bool {
        matches!(self, ContentMode::TitleLink | ContentMode::TitleOnly)
    }

    pub fn single_media(&self) -> bool {
        matches!(self, ContentMode::TitleSummary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceDisplay {
    Source,
    Author,
    None,
}

impl SourceDisplay {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "author" => Self::Author,
            "none" => Self::None,
            _ => Self::Source,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub mode: ContentMode,
    pub source: SourceDisplay,
    pub link_preview: bool,
    pub lang: Lang,
    pub allow_rich: bool,
}

async fn rich_source_for_path(
    path: &std::path::Path,
    transport: &crate::media::MediaTransport,
) -> anyhow::Result<RichMediaSource> {
    if let Some(bp) = transport.bot_path(path) {
        return Ok(RichMediaSource::FilePath(bp));
    }
    tokio::fs::metadata(path).await?;
    Ok(RichMediaSource::File {
        path: path.to_path_buf(),
        filename: crate::media::logical_file_name(path),
    })
}

// One owner for the file_id cache key layout: <content hash>:<send mode>.
const FILE_ID_DOC_MODE: &str = "doc";

fn file_id_key(content_hash: &str, mode: &str) -> String {
    format!("{}:{}", content_hash, mode)
}

async fn rich_source_for_item(
    db: &Db,
    item: &MediaItem,
    transport: &crate::media::MediaTransport,
    mode: &str,
) -> anyhow::Result<RichMediaSource> {
    let key = file_id_key(&item.content_hash, mode);
    if let Ok(Some(fid)) = db::get_cached_file_id(db, &key).await {
        return Ok(RichMediaSource::FileId(fid));
    }
    rich_source_for_path(&item.path, transport).await
}

async fn rich_source_for_doc(db: &Db, item: &MediaItem) -> anyhow::Result<RichMediaSource> {
    let key = file_id_key(&item.content_hash, FILE_ID_DOC_MODE);
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
            disable_content_type_detection: item.kind == MediaKind::Image,
        });
    }

    Ok(out)
}

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
    let html = rich_html(body, footer, "", true);
    send_rich_once(bot, chat_id, html, Vec::new()).await
}

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
    let html = rich_html(text, "", &media_html, caption_above_media);
    let media = build_rich_media_specs(db, album_media, docs, transport).await?;
    send_rich_once(bot, chat_id, html, media).await
}

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

async fn resolve_input(
    db: &Db,
    item: &MediaItem,
    mode: &str,
    use_cache: bool,
) -> anyhow::Result<InputFile> {
    if use_cache {
        let key = file_id_key(&item.content_hash, mode);
        if let Ok(Some(fid)) = db::get_cached_file_id(db, &key).await {
            return Ok(InputFile::file_id(teloxide::types::FileId(fid)));
        }
    }
    path_input(&item.path).await
}

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

fn group_item_mode(m: &MediaItem) -> &'static str {
    match m.kind {
        MediaKind::Video => "video",
        _ if m.is_gif => "gif",
        _ => "photo",
    }
}

async fn cache_file_id(db: &Db, item: &MediaItem, mode: &str, file_id: Option<String>) {
    if let Some(fid) = file_id {
        let key = file_id_key(&item.content_hash, mode);
        db::put_cached_file_id(db, &key, &fid).await.ok();
    }
}

async fn invalidate_op_cache(db: &Db, op: &RichOp<'_>) {
    let items: Vec<(&MediaItem, &str)> = match op {
        RichOp::Photo(p) => vec![(p, "photo")],
        RichOp::Gif(g) => vec![(g, "gif")],
        RichOp::Video(v) => vec![(v, "video")],
        RichOp::Group(items) => items.iter().map(|m| (*m, group_item_mode(m))).collect(),
    };
    for (item, mode) in items {
        let key = file_id_key(&item.content_hash, mode);
        db::invalidate_cached_file_id(db, &key).await.ok();
    }
}

pub struct PushCtx<'a> {
    pub entry: &'a Entry,
    pub feed_title: &'a str,
    pub author: Option<&'a str>,
    pub opts: RenderOpts,
    pub caption_above_media: bool,
    pub media: &'a [MediaItem],
    pub meta: Option<&'a crate::entrymeta::EntryMeta>,
}

fn split_rich_docs<'a>(docs: &[&'a MediaItem]) -> (Vec<&'a MediaItem>, Vec<&'a MediaItem>) {
    let mut rich = Vec::new();
    let mut follow = Vec::new();
    for item in docs {
        if item.kind == MediaKind::Image && !item.is_retweet && !item.is_gif {
            rich.push(*item);
        } else {
            follow.push(*item);
        }
    }
    (rich, follow)
}

struct MediaPlan<'a> {
    photos: Vec<&'a MediaItem>,
    gifs: Vec<&'a MediaItem>,
    docs: Vec<&'a MediaItem>,
    videos: Vec<&'a MediaItem>,
    rt_photos: Vec<&'a MediaItem>,
}

fn classify_media<'a>(media: &'a [MediaItem]) -> MediaPlan<'a> {
    let mut photos: Vec<&MediaItem> = Vec::new();
    let mut gifs: Vec<&MediaItem> = Vec::new();
    let mut docs: Vec<&MediaItem> = Vec::new();
    let mut videos: Vec<&MediaItem> = Vec::new();
    let mut rt_photos: Vec<&MediaItem> = Vec::new();
    let aspect_threshold = crate::config::LONG_IMAGE_ASPECT_RATIO;
    for m in media {
        if m.is_retweet && m.kind == MediaKind::Image {
            if m.is_gif {
                gifs.push(m);
            } else if crate::media::image_sends_as_document(m, aspect_threshold) {
                docs.push(m);
            } else {
                rt_photos.push(m);
            }
            continue;
        }
        match m.kind {
            MediaKind::Image => {
                if m.is_gif {
                    gifs.push(m);
                } else if crate::media::image_sends_as_document(m, aspect_threshold) {
                    docs.push(m);
                    tracing::debug!(
                        size = m.size,
                        w = m.width,
                        h = m.height,
                        "image -> document"
                    );
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

fn doc_fallback_allowed(err_str: &str) -> bool {
    !crate::sendpolicy::is_ambiguous_send_error(err_str)
}

// A replay of the body is safe only when the first media op is known not to have been delivered.
fn media_fallback_allowed(sent: usize, err_str: &str) -> bool {
    sent > 0 || doc_fallback_allowed(err_str)
}

struct TailDelivery {
    anchor: Option<Message>,
    first_doc: Option<Message>,
    caption_delivered: bool,
    media_failed: bool,
}

impl TailDelivery {
    fn new(anchor: Option<Message>, caption_delivered: bool, media_failed: bool) -> Self {
        Self {
            anchor,
            first_doc: None,
            caption_delivered,
            media_failed,
        }
    }

    fn has_carrier(&self) -> bool {
        self.anchor.is_some() || self.first_doc.is_some()
    }

    fn reply_to(&self) -> Option<teloxide::types::MessageId> {
        self.anchor
            .as_ref()
            .or(self.first_doc.as_ref())
            .map(|m| m.id)
    }

    async fn send_documents(
        &mut self,
        bot: &Bot,
        db: &Db,
        chat_id: i64,
        docs: &[&MediaItem],
        caption: &str,
        group: bool,
    ) {
        if docs.is_empty() {
            return;
        }
        let reply_to = self.reply_to();
        let cap = if self.has_carrier() { "" } else { caption };

        if group {
            match send_doc_group(bot, db, chat_id, docs, cap, reply_to).await {
                Ok(msgs) => {
                    self.first_doc = msgs.into_iter().next();
                }
                Err((delivered, e)) => {
                    let err_str = error_chain_string(e.as_ref());
                    if !doc_fallback_allowed(&err_str) {
                        tracing::warn!(
                            chat_id = chat_id,
                            error = %e,
                            delivered = delivered.len(),
                            total_docs = docs.len(),
                            "doc group outcome uncertain, not retrying remaining documents"
                        );
                        if let Some(first) = delivered.first() {
                            self.first_doc = Some(first.clone());
                        }
                        self.media_failed = true;
                        return;
                    }
                    tracing::warn!(
                        chat_id = chat_id,
                        error = %e,
                        delivered = delivered.len(),
                        total_docs = docs.len(),
                        "doc group send failed, falling back to one-by-one"
                    );
                    let mut chain_id = reply_to;
                    if let Some(first) = delivered.first() {
                        self.first_doc = Some(first.clone());
                        if self.anchor.is_none() && chain_id.is_none() {
                            chain_id = Some(first.id);
                        }
                    }
                    for (i, d) in docs.iter().enumerate().skip(delivered.len()) {
                        let cap = if i == 0 && self.anchor.is_none() {
                            cap
                        } else {
                            ""
                        };
                        match send_doc_msg(bot, db, chat_id, d, cap, chain_id).await {
                            Ok(m) => {
                                if self.first_doc.is_none() {
                                    self.first_doc = Some(m.clone());
                                }
                                if self.anchor.is_none() && chain_id.is_none() {
                                    chain_id = Some(m.id);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(chat_id = chat_id, error = %e, "document send failed");
                                self.media_failed = true;
                                break;
                            }
                        }
                    }
                }
            }
            return;
        }

        let mut reply_to = reply_to;
        let mut cap = cap;
        for d in docs {
            match send_doc_msg(bot, db, chat_id, d, cap, reply_to).await {
                Ok(m) => {
                    if self.first_doc.is_none() {
                        self.first_doc = Some(m.clone());
                    }
                    if reply_to.is_none() {
                        reply_to = Some(m.id);
                    }
                    cap = "";
                }
                Err(e) => {
                    tracing::warn!(chat_id = chat_id, error = %e, "document send failed");
                    self.media_failed = true;
                    break;
                }
            }
        }
    }

    async fn send_retweet_images(
        &mut self,
        bot: &Bot,
        db: &Db,
        chat_id: i64,
        rt_photos: &[&MediaItem],
        caption: &str,
        caption_above: bool,
    ) {
        if rt_photos.is_empty() {
            return;
        }
        let rt_is_sole_carrier = !self.has_carrier();
        let rt_caption = if rt_is_sole_carrier { caption } else { "" };
        let rt_reply_to = self.reply_to();
        let rt_ops = build_rich_ops(rt_photos, &[], &[]);
        match send_rich_ops_with_retry(
            bot,
            db,
            chat_id,
            rt_caption,
            caption_above,
            &rt_ops,
            rt_reply_to,
            "rt_images",
        )
        .await
        {
            Ok((_, first_rt_msg)) => {
                if rt_is_sole_carrier {
                    if let Some(m) = first_rt_msg {
                        self.anchor = Some(m);
                    }
                }
            }
            Err((sent, delivered, e)) => {
                tracing::warn!(
                    chat_id = chat_id,
                    failed_op = sent,
                    total_ops = rt_ops.len(),
                    error = %e,
                    "retweet inline images send failed"
                );
                self.media_failed = true;
                if rt_is_sole_carrier && sent > 0 {
                    self.caption_delivered = true;
                    if self.anchor.is_none() {
                        self.anchor = delivered.map(|m| *m);
                    }
                }
            }
        }
    }

    async fn send_overflow(
        &mut self,
        bot: &Bot,
        chat_id: i64,
        chunks: &[String],
    ) -> anyhow::Result<()> {
        let reply_to = self.reply_to();
        for chunk in chunks {
            if let Err(e) = send_chunk_with_retry(bot, chat_id, chunk, false, reply_to).await {
                if self.caption_delivered || self.has_carrier() {
                    tracing::warn!(
                        chat_id = chat_id,
                        error = %e,
                        "overflow text failed after delivery, not retrying"
                    );
                    self.media_failed = true;
                    break;
                }
                return Err(e);
            }
        }
        Ok(())
    }

    async fn send_poll(
        &mut self,
        bot: &Bot,
        chat_id: i64,
        meta: Option<&crate::entrymeta::EntryMeta>,
    ) {
        if try_send_poll(bot, chat_id, self.reply_to(), meta).await {
            self.media_failed = true;
        }
    }

    fn into_outcome(
        self,
        entry: &Entry,
        text_only: bool,
        text_partial: bool,
        uncertain: bool,
    ) -> SendOutcome {
        let delivered_tg_link = self
            .anchor
            .as_ref()
            .and_then(tg_message_link)
            .or_else(|| self.first_doc.as_ref().and_then(tg_message_link));
        let delivered_message_id = self
            .anchor
            .as_ref()
            .or(self.first_doc.as_ref())
            .map(|m| m.id.0 as i64);
        SendOutcome {
            media_failed: self.media_failed,
            entry_link: fetcher::entry_link(entry),
            delivered_tg_link,
            delivered_message_id,
            text_only,
            uncertain,
            text_partial,
        }
    }
}

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
    let mut tail = TailDelivery::new(delivered.cloned(), sent > 0, true);
    let text_only = sent == 0;
    let mut text_delivered = sent > 0;
    let mut text_partial = false;
    if sent > 0 {
        tail.send_overflow(bot, chat_id, overflow_chunks).await?;
    } else if let Some((body, footer)) = format_entry_parts(entry, feed_title, author, opts, meta) {
        match send_entry_msg(bot, chat_id, &body, &footer, opts.link_preview).await {
            Ok(sent) => {
                tail.anchor = Some(sent.first);
                tail.caption_delivered = true;
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
    tail.send_documents(bot, db, chat_id, docs, "", false).await;
    tail.send_retweet_images(bot, db, chat_id, rt_photos, "", false)
        .await;
    tail.send_poll(bot, chat_id, meta).await;
    Ok(tail.into_outcome(entry, text_only, text_partial, false))
}

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
        media,
        meta,
    } = ctx;
    let full_caption = format_caption(entry, feed_title, author, opts, meta);
    let caption_above = caption_above_media;

    let media: &[MediaItem] = if opts.mode == ContentMode::TitleSummary {
        match media.iter().position(|m| m.kind == MediaKind::Image) {
            Some(i) => &media[i..i + 1],
            None => media,
        }
    } else {
        media
    };

    if media.is_empty() {
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
    } = classify_media(media);

    let rich_ops = build_rich_ops(&photos, &gifs, &videos);
    let mut tail = TailDelivery::new(None, false, false);
    let mut rich_delivered = false;

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
        let rich_caption = format_caption_with_mode(
            entry,
            feed_title,
            author,
            opts,
            meta,
            fetcher::TableMode::Rich,
        );
        match try_send_rich_media(
            bot,
            db,
            chat_id,
            &rich_caption,
            caption_above,
            &album_media,
            &rich_docs,
            transport,
        )
        .await
        {
            Ok(m) => {
                tail.anchor = Some(m);
                tail.caption_delivered = true;
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

    if rich_delivered && !gifs.is_empty() {
        let gif_ops = build_rich_ops(&[], &gifs, &[]);
        let reply_to = tail.reply_to();
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
            tail.media_failed = true;
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
                tail.anchor = first_msg;
                tail.caption_delivered = true;
            }
            Err((sent, delivered, e)) => {
                let err_str = error_chain_string(e.as_ref());
                tracing::warn!(
                    chat_id = chat_id,
                    entry_id = %fetcher::entry_id(entry),
                    failed_op = sent,
                    total_ops = rich_ops.len(),
                    error = %err_str,
                    "media send failed, falling back to text"
                );
                if !media_fallback_allowed(sent, &err_str) {
                    tracing::warn!(
                        chat_id = chat_id,
                        entry_id = %fetcher::entry_id(entry),
                        "media send outcome uncertain; not falling back to text"
                    );
                    return Ok(SendOutcome {
                        entry_link: fetcher::entry_link(entry),
                        uncertain: true,
                        ..SendOutcome::default()
                    });
                }
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

    let docs_to_send: &[&MediaItem] = if rich_delivered { &follow_docs } else { &docs };
    tail.send_documents(bot, db, chat_id, docs_to_send, &caption, true)
        .await;

    tail.send_retweet_images(bot, db, chat_id, &rt_photos, &caption, caption_above)
        .await;

    if !rich_delivered {
        tail.send_overflow(bot, chat_id, &overflow_chunks).await?;
    }

    if !tail.caption_delivered && !tail.has_carrier() && (!docs.is_empty() || !rt_photos.is_empty())
    {
        return Err(anyhow::anyhow!(
            "all caption carriers (documents/retweet images) failed to send; caption undelivered"
        ));
    }

    tail.send_poll(bot, chat_id, meta).await;
    Ok(tail.into_outcome(entry, false, false, false))
}

enum RichOp<'a> {
    Photo(&'a MediaItem),
    Gif(&'a MediaItem),
    Video(&'a MediaItem),
    Group(Vec<&'a MediaItem>),
}

fn build_rich_ops<'a>(
    photos: &[&'a MediaItem],
    gifs: &[&'a MediaItem],
    videos: &[&'a MediaItem],
) -> Vec<RichOp<'a>> {
    let mut ops: Vec<RichOp<'a>> = Vec::new();

    let mut pool: Vec<&'a MediaItem> = Vec::with_capacity(videos.len() + photos.len());
    pool.extend_from_slice(videos);
    pool.extend_from_slice(photos);

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

    for g in gifs {
        ops.push(RichOp::Gif(g));
    }

    ops
}

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

    if docs.len() == 1 {
        let m = send_doc_msg(bot, db, chat_id, docs[0], caption, reply_to)
            .await
            .map_err(|e| (Vec::new(), e))?;
        return Ok(vec![m]);
    }

    let mut all_msgs: Vec<Message> = Vec::new();
    let mut first_msg_id: Option<teloxide::types::MessageId> = None;

    for (gi, chunk) in docs.chunks(ALBUM_LIMIT).enumerate() {
        let group_caption = if gi == 0 { caption } else { "" };
        let reply_msg_id = if gi > 0 { first_msg_id } else { reply_to };
        let rp = reply_msg_id.map(teloxide::types::ReplyParameters::new);

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
            .map(|d| file_id_key(&d.content_hash, FILE_ID_DOC_MODE))
            .collect();
        let has_thumb = chunk.iter().any(|d| d.thumb_path.is_some());
        let msgs = send_with_full_retry(db, chat_id, &keys, has_thumb, |use_cache, use_thumbs| {
            let rp = rp.clone();
            async move {
                let mut group: Vec<InputMedia> = Vec::new();
                for (j, d) in chunk.iter().enumerate() {
                    let inp = resolve_input(db, d, "doc", use_cache).await?;
                    let mut im = InputMediaDocument::new(inp);
                    if d.kind == MediaKind::Image {
                        im.disable_content_type_detection = Some(true);
                    }
                    if j == 0 && !group_caption.is_empty() {
                        im = im
                            .caption(group_caption.to_string())
                            .parse_mode(ParseMode::Html);
                    }
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

async fn send_doc_msg(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    d: &MediaItem,
    caption: &str,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    pace(chat_id).await;
    let caption = normalize_blockquote_fold(caption);
    let caption = caption.as_str();
    let key = file_id_key(&d.content_hash, FILE_ID_DOC_MODE);
    let m = send_with_full_retry(
        db,
        chat_id,
        std::slice::from_ref(&key),
        d.thumb_path.is_some(),
        |use_cache, attach_thumb| async move {
            let inp = resolve_input(db, d, "doc", use_cache).await?;
            let mut req = bot.send_document(ChatId(chat_id), inp);
            // Image documents must opt out of content-type detection; otherwise WebP becomes a sticker and loses the caption.
            if d.kind == MediaKind::Image {
                req = req.disable_content_type_detection(true);
            }
            if attach_thumb {
                if let Some(tp) = &d.thumb_path {
                    if let Ok(tin) = path_input(tp).await {
                        req = req.thumbnail(tin);
                    }
                }
            }
            if !caption.is_empty() {
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

type RichSendFailure = (usize, Option<Box<Message>>, anyhow::Error);

#[allow(clippy::too_many_arguments)]
async fn send_rich_ops(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    caption: &str,
    caption_above: bool,
    ops: &[RichOp<'_>],
    resume: usize,
    reply_first_to: Option<teloxide::types::MessageId>,
    resume_anchor: Option<teloxide::types::MessageId>,
) -> Result<(usize, Option<Message>), RichSendFailure> {
    let mut first_msg: Option<Message> = None;
    for (i, op) in ops.iter().enumerate().skip(resume) {
        pace(chat_id).await;
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
        Ok((total, first)) => Ok((total, first_delivered.map(|m| *m).or(first))),
        Err((sent, first, e)) => Err((sent, first_delivered.or(first), e)),
    }
}

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
    let caption_owned = normalize_blockquote_fold(caption);
    let caption = caption_owned.as_str();
    let reply_params = reply_to.map(teloxide::types::ReplyParameters::new);
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
            for (m, item) in msgs.iter().zip(items.iter()) {
                let mode = group_item_mode(item);
                cache_file_id(db, item, mode, message_file_id_for(m, mode)).await;
            }
            Ok(msgs)
        }
    }
}

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
                im.show_caption_above_media = caption_above;
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

pub struct TextSendOutcome {
    pub first: Message,
    pub partial: bool,
}

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
        let text = if i == last_idx && !footer.is_empty() {
            format!("{}\n\n{}", chunk, footer)
        } else {
            chunk.clone()
        };
        let lp = if i == 0 { link_preview } else { false };
        let reply_to = first_msg.as_ref().map(|m| m.id);
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
    }
    Ok(TextSendOutcome {
        first: first_msg.expect("at least one chunk"),
        partial: false,
    })
}

async fn send_single_html(
    bot: &Bot,
    chat_id: i64,
    html_text: &str,
    link_preview: bool,
    reply_to: Option<teloxide::types::MessageId>,
) -> anyhow::Result<Message> {
    let html_text = normalize_blockquote_fold(html_text);
    let lpo = if link_preview {
        None
    } else {
        Some(no_preview())
    };
    let mut retry = SendRetryState::new(SendRetryPolicy::TEXT, false, false);
    let mut plain_text: Option<String> = None;

    loop {
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
                        tracing::warn!(
                            chat_id = chat_id,
                            error = %err_str,
                            html_head = %html_text.chars().take(100).collect::<String>(),
                            "HTML parse failed, falling back to plain text"
                        );
                        plain_text = Some(crate::htmltext::strip_html_keep_links(
                            &html_text, SAFE_LIMIT,
                        ));
                        continue;
                    }
                    _ => return Err(e.into()),
                }
            }
        }
    }
}

pub(crate) async fn send_chunk_with_retry(
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

pub fn tg_message_link(msg: &Message) -> Option<String> {
    let cid = msg.chat.id.0;
    let bare = if cid < 0 {
        let s = cid.to_string();
        let x = s.strip_prefix("-100")?;
        x.to_string()
    } else {
        return None;
    };
    Some(format!("https://t.me/c/{}/{}", bare, msg.id.0))
}

#[cfg(test)]
mod tests {
    use super::render::*;
    use super::*;
    use crate::htmltext::truncate_html_keep_links;

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
    fn doc_group_fallback_rejects_ambiguous_outcomes() {
        assert!(!doc_fallback_allowed("request timed out after 600s"));
        assert!(!doc_fallback_allowed("Telegram API error 502: Bad Gateway"));
        assert!(doc_fallback_allowed(
            "Telegram API error 400: Bad Request: chat not found"
        ));
    }

    #[test]
    fn media_fallback_blocked_only_before_first_op() {
        assert!(!media_fallback_allowed(0, "request timed out after 600s"));
        assert!(!media_fallback_allowed(
            0,
            "Telegram API error 502: Bad Gateway"
        ));
        assert!(media_fallback_allowed(
            0,
            "Telegram API error 400: Bad Request: chat not found"
        ));
        assert!(media_fallback_allowed(1, "request timed out after 600s"));
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
        let opts: Vec<&str> = (0..12).map(|_| "选项").collect();
        let meta = meta_with_poll("多选", opts, false);
        let (_, options, _) = extract_poll(Some(&meta)).unwrap();
        assert_eq!(options.len(), 10);
    }

    #[test]
    fn extract_poll_rejects_single_option() {
        let meta = meta_with_poll("问题", vec!["唯一"], false);
        assert!(extract_poll(Some(&meta)).is_none());
    }

    #[test]
    fn extract_poll_no_meta() {
        assert!(extract_poll(None).is_none());
    }

    #[test]
    fn extract_poll_meta_without_poll() {
        let meta = crate::entrymeta::EntryMeta::default();
        assert!(extract_poll(Some(&meta)).is_none());
    }

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
    fn rich_mode_renders_native_table_without_changing_standard_render() {
        let body = concat!(
            "<p>正文</p>",
            "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>"
        );
        let entry = rss_entry_with_body(body);
        let opts = RenderOpts {
            mode: ContentMode::TitleContent,
            source: SourceDisplay::None,
            link_preview: false,
            lang: Lang::Zh,
            allow_rich: true,
        };

        let (standard, _) = format_entry_parts(&entry, "feed", None, opts, None).unwrap();
        assert!(!standard.contains("<table"), "standard: {standard}");

        let (rich, _) = format_entry_parts_with_mode(
            &entry,
            "feed",
            None,
            opts,
            None,
            crate::fetcher::TableMode::Rich,
        )
        .unwrap();
        assert!(rich.contains("<table bordered>"), "rich: {rich}");
        assert!(
            !should_try_rich_text(standard_text_len(&rich, "")),
            "short table must not trigger rich"
        );
    }

    #[test]
    fn entry_summary_keeps_wechat_account_link_clickable() {
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
        assert!(
            summary.contains("微信公众号：<a href=\"https://mp.weixin.qq.com/s?x=1&amp;y=2\">示例公众号</a>，作者"),
            "anchor misplaced: {summary}"
        );
        assert!(summary.contains("正文开始"), "body lost: {summary}");
    }

    #[test]
    fn entry_summary_skips_inline_style_source() {
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
        let body = "<p>成本&lt;100&amp;更多&gt;一些文字继续更多内容截断部分。</p>";
        let summary = entry_summary(&rss_entry_with_body(body), 7, None);
        assert_eq!(
            summary, "成本&lt;100&amp;......",
            "entity cut wrong: {summary}"
        );
        let summary = entry_summary(&rss_entry_with_body(body), 6, None);
        assert_eq!(summary, "成本&lt;100......", "entity cut wrong: {summary}");
    }

    #[test]
    fn entry_summary_keeps_video_anchor() {
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

        let text = "中文".repeat(10);
        let cut = truncate_html_input(&text, 5);
        assert_eq!(cut, "中");

        assert_eq!(truncate_html_input("<img src=\"unfinished", 8), "");
    }

    #[test]
    fn entry_summary_large_body_uses_prefix_without_changing_output() {
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
        let body = format!(
            "{}<p>后面的正文</p>",
            "<img src=\"https://example.com/x\">".repeat(20_000)
        );
        let summary = entry_summary(&rss_entry_with_body(&body), 140, None);
        assert_eq!(summary, "后面的正文");
    }

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
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let ops = build_rich_ops(&[&p], &[], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1]"]);
    }

    #[test]
    fn rich_ops_video_leads_image_within_merged_album() {
        let v = mk_item("v1", MediaKind::Video, false);
        let p1 = mk_item("p1", MediaKind::Image, false);
        let p2 = mk_item("p2", MediaKind::Image, false);
        let ops = build_rich_ops(&[&p1, &p2], &[], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1+p2]"]);
    }

    #[test]
    fn rich_ops_single_item_falls_back_to_direct_send() {
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
        let v = mk_item("v1", MediaKind::Video, false);
        let p = mk_item("p1", MediaKind::Image, false);
        let g1 = mk_item("g1", MediaKind::Image, true);
        let g2 = mk_item("g2", MediaKind::Image, true);
        let ops = build_rich_ops(&[&p], &[&g1, &g2], &[&v]);
        assert_eq!(op_labels(&ops), vec!["group[v1+p1]", "gif:g1", "gif:g2"]);
    }

    #[test]
    fn rich_ops_overflow_splits_into_follow_up_albums() {
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
        let body_4094 = "x".repeat(SAFE_LIMIT - 2);
        let body_4095 = "x".repeat(SAFE_LIMIT - 1);
        assert_eq!(standard_text_len(&body_4094, ""), SAFE_LIMIT);
        assert!(!should_try_rich_text(standard_text_len(&body_4094, "")));
        assert_eq!(standard_text_len(&body_4095, ""), SAFE_LIMIT + 1);
        assert!(should_try_rich_text(standard_text_len(&body_4095, "")));

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
        assert!(!should_try_rich_media(CAPTION_LIMIT, 10));
        assert!(should_try_rich_media(CAPTION_LIMIT + 1, 10));
        assert!(should_try_rich_media(0, 11));
        assert!(should_try_rich_media(CAPTION_LIMIT + 1, 1));
        assert!(!should_try_rich_media(CAPTION_LIMIT + 1, 0));
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

        assert_eq!(
            rich_media_html(&[], &[&doc_image, &doc_image2]),
            "<tg-document src=\"tg://document?id=m0\"></tg-document>\
             <tg-document src=\"tg://document?id=m1\"></tg-document>"
        );
    }

    #[test]
    fn split_rich_docs_keeps_audio_files_gifs_and_retweets_out() {
        let doc_image = mk_item("d0", MediaKind::Image, false);
        let audio = mk_item("d1", MediaKind::Other, false);
        let file = mk_item("d2", MediaKind::Other, false);
        let gif = mk_item("d3", MediaKind::Image, true);
        let mut retweet = mk_item("d4", MediaKind::Image, false);
        retweet.is_retweet = true;

        let (rich, follow) = split_rich_docs(&[&doc_image, &audio, &file, &gif, &retweet]);

        assert_eq!(rich.len(), 1);
        assert_eq!(rich[0].content_hash, "d0");
        assert_eq!(follow.len(), 4);
        assert_eq!(follow[0].content_hash, "d1");
        assert_eq!(follow[1].content_hash, "d2");
        assert_eq!(follow[2].content_hash, "d3");
        assert_eq!(follow[3].content_hash, "d4");
    }

    #[test]
    fn oversized_gif_remains_animation_not_document() {
        let mut gif = mk_item("g0", MediaKind::Image, true);
        gif.size = 20 * 1024 * 1024;
        gif.path = std::path::PathBuf::from("g0.bin");
        let media = [gif];
        let plan = classify_media(&media);
        assert_eq!(plan.gifs.len(), 1);
        assert!(plan.docs.is_empty());
        assert!(plan.photos.is_empty());
    }

    #[test]
    fn rich_media_html_never_groups_documents_into_slideshow() {
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
