//! Minimal raw wrapper for Bot API `sendRichMessage`.
//!
//! teloxide 0.17 models Bot API 9.1 and has no `sendRichMessage` types, while
//! the official API / local Bot API server already expose Bot API 10.x. The
//! crate still exposes `Bot::client/token/api_url`, so this module performs
//! the JSON/multipart request itself and keeps the new API surface isolated
//! instead of forking or upgrading the whole framework.
//!
//! Only the subset needed by feed pushes is modelled here: rich HTML plus
//! photo/video attachments. Rich media is an enhancement; callers must fall
//! back to the normal send path when this module returns an error.

use reqwest_bot::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use teloxide::prelude::Bot;
use teloxide::types::{Message, MessageId};

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<i32>,
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponseParameters {
    retry_after: Option<u64>,
}

async fn parse_response<T: for<'de> Deserialize<'de>>(
    response: reqwest_bot::Response,
) -> anyhow::Result<T> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("rich message response read failed: {}", e.without_url()))?;
    let parsed: TelegramResponse<T> = serde_json::from_str(&body).map_err(|e| {
        anyhow::anyhow!(
            "rich message response parse failed (HTTP {}): {}",
            status.as_u16(),
            e
        )
    })?;
    if parsed.ok {
        return parsed
            .result
            .ok_or_else(|| anyhow::anyhow!("rich message response missing result"));
    }

    let mut description = parsed
        .description
        .unwrap_or_else(|| "unknown Telegram API error".to_string());
    if let Some(retry_after) = parsed.parameters.and_then(|p| p.retry_after) {
        description.push_str(&format!(" (retry after {retry_after}s)"));
    }
    anyhow::bail!(
        "Telegram API error {}: {}",
        parsed.error_code.unwrap_or(status.as_u16() as i32),
        description
    )
}

fn method_url(bot: &Bot, method: &str) -> String {
    format!(
        "{}/bot{}/{method}",
        bot.api_url().as_str().trim_end_matches('/'),
        bot.token()
    )
}

/// Where one rich-message media attachment comes from.
#[derive(Debug, Clone)]
pub enum RichMediaSource {
    /// Reuse an already uploaded file id.
    FileId(String),
    /// Local Bot API server reads this path from its own filesystem namespace
    /// (shared mount). The value is already translated to the server prefix.
    FilePath(String),
    /// Official API (or an untranslatable path): upload the bytes through
    /// multipart/form-data.
    Bytes { data: Vec<u8>, filename: String },
}

/// Media type accepted by `InputRichMessageMedia`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RichMediaKind {
    Photo,
    Video,
}

/// One media entry inside `InputRichMessage.media`.
#[derive(Debug, Clone)]
pub struct RichMedia {
    /// Referenced from rich HTML as `tg://photo?id=<id>` / `tg://video?id=<id>`.
    pub id: String,
    pub kind: RichMediaKind,
    pub source: RichMediaSource,
    /// Optional video thumbnail. Uploaded/prepared with the same transport
    /// rules as the media itself.
    pub thumbnail: Option<RichMediaSource>,
    pub width: u32,
    pub height: u32,
    pub duration: Option<u32>,
}

fn value_and_part(source: RichMediaSource, part_name: &str) -> (String, Option<Part>) {
    match source {
        RichMediaSource::FileId(id) => (id, None),
        RichMediaSource::FilePath(path) => (format!("file://{path}"), None),
        RichMediaSource::Bytes { data, filename } => {
            let part = Part::bytes(data).file_name(filename);
            (format!("attach://{part_name}"), Some(part))
        }
    }
}

/// Send one rich message. `html` must already be valid rich HTML; `media`
/// contains photo/video attachments referenced by `tg://...?id=` links.
pub async fn send_rich_message(
    bot: &Bot,
    chat_id: i64,
    html: String,
    media: Vec<RichMedia>,
    reply_to: Option<MessageId>,
) -> anyhow::Result<Message> {
    if html.trim().is_empty() && media.is_empty() {
        anyhow::bail!("empty rich message");
    }

    let mut media_json: Vec<Value> = Vec::with_capacity(media.len());
    let mut parts: Vec<(String, Part)> = Vec::new();
    for item in media {
        let RichMedia {
            id,
            kind,
            source,
            thumbnail,
            width,
            height,
            duration,
        } = item;

        let (media_value, media_part) = value_and_part(source, &id);
        if let Some(part) = media_part {
            parts.push((id.clone(), part));
        }

        // InputRichMessageMedia = { id, media: InputMediaPhoto|InputMediaVideo }.
        // The media type and its properties belong to the nested `media`
        // object, not to the outer item.
        let mut media_object = Map::new();
        media_object.insert(
            "type".to_string(),
            Value::String(
                match kind {
                    RichMediaKind::Photo => "photo",
                    RichMediaKind::Video => "video",
                }
                .to_string(),
            ),
        );
        media_object.insert("media".to_string(), Value::String(media_value));

        if kind == RichMediaKind::Video {
            if width > 0 {
                media_object.insert("width".to_string(), json!(width));
            }
            if height > 0 {
                media_object.insert("height".to_string(), json!(height));
            }
            if let Some(duration) = duration {
                media_object.insert("duration".to_string(), json!(duration));
            }
            media_object.insert("supports_streaming".to_string(), json!(true));

            if let Some(thumb) = thumbnail {
                let thumb_name = format!("thumb_{id}");
                let (thumb_value, thumb_part) = value_and_part(thumb, &thumb_name);
                if let Some(part) = thumb_part {
                    parts.push((thumb_name, part));
                }
                media_object.insert("thumbnail".to_string(), Value::String(thumb_value));
            }
        }

        let mut item_object = Map::new();
        item_object.insert("id".to_string(), Value::String(id));
        item_object.insert("media".to_string(), Value::Object(media_object));
        media_json.push(Value::Object(item_object));
    }

    let mut rich = Map::new();
    rich.insert("html".to_string(), Value::String(html));
    if !media_json.is_empty() {
        rich.insert("media".to_string(), Value::Array(media_json));
    }
    let rich = Value::Object(rich);

    let mut payload = Map::new();
    payload.insert("chat_id".to_string(), json!(chat_id));
    payload.insert("rich_message".to_string(), rich.clone());
    if let Some(id) = reply_to {
        payload.insert("reply_parameters".to_string(), json!({ "message_id": id.0 }));
    }
    let payload = Value::Object(payload);

    let url = method_url(bot, "sendRichMessage");
    if parts.is_empty() {
        let response = bot
            .client()
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("rich message request failed: {}", e.without_url()))?;
        return parse_response::<Message>(response).await;
    }

    let rich_json = serde_json::to_string(&rich)?;
    let mut form = Form::new()
        .text("chat_id", chat_id.to_string())
        .text("rich_message", rich_json);
    if let Some(id) = reply_to {
        form = form.text(
            "reply_parameters",
            serde_json::to_string(&json!({ "message_id": id.0 }))?,
        );
    }
    for (name, part) in parts {
        form = form.part(name, part);
    }

    let response = bot
        .client()
        .post(url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("rich message upload failed: {}", e.without_url()))?;
    parse_response::<Message>(response).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_and_file_path_do_not_require_parts() {
        let (value, part) = value_and_part(RichMediaSource::FileId("abc".into()), "m0");
        assert_eq!(value, "abc");
        assert!(part.is_none());

        let (value, part) =
            value_and_part(RichMediaSource::FilePath("/tmp/a.jpg".into()), "m0");
        assert_eq!(value, "file:///tmp/a.jpg");
        assert!(part.is_none());
    }

    #[test]
    fn bytes_use_attach_reference() {
        let (value, part) = value_and_part(
            RichMediaSource::Bytes {
                data: vec![1, 2, 3],
                filename: "a.jpg".into(),
            },
            "m0",
        );
        assert_eq!(value, "attach://m0");
        assert!(part.is_some());
    }

    #[tokio::test]
    async fn sends_json_payload_to_the_expected_method_url() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let mut data = Vec::new();
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                if let Some(head_end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..head_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if data.len() >= head_end + 4 + content_length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&data);
            assert!(
                request.starts_with("POST /bot123:abc/sendRichMessage HTTP/1.1"),
                "unexpected request line: {request}"
            );
            assert!(request.contains("\"chat_id\":42"));
            assert!(request.contains("\"html\":\"hello\""));
            assert!(request.contains("\"id\":\"m0\""));
            // InputRichMessageMedia.media is an InputMediaPhoto object.
            assert!(request.contains("\"media\":{\"media\":\"file-id\",\"type\":\"photo\"}"));
            assert!(request.contains("\"reply_parameters\""));
            assert!(request.contains("\"message_id\":9"));

            let body = r#"{"ok":true,"result":{"message_id":7,"date":1770000000,"chat":{"id":42,"type":"channel","title":"t"}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let client = reqwest_bot::Client::builder().no_proxy().build().unwrap();
        let bot = Bot::with_client("123:abc", client)
            .set_api_url(format!("http://{addr}").parse().unwrap());
        let message = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            send_rich_message(
                &bot,
                42,
                "hello".into(),
                vec![RichMedia {
                    id: "m0".into(),
                    kind: RichMediaKind::Photo,
                    source: RichMediaSource::FileId("file-id".into()),
                    thumbnail: None,
                    width: 0,
                    height: 0,
                    duration: None,
                }],
                Some(MessageId(9)),
            ),
        )
        .await
        .expect("client timed out")
        .unwrap();
        assert_eq!(message.id.0, 7);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("server timed out")
            .unwrap();
    }

    #[test]
    fn teloxide_message_parses_rich_message_response() {
        // teloxide 0.17 has no rich-message types. The response is still a
        // normal Message envelope; unknown rich fields must be ignored and the
        // message id/chat must remain usable for /find, polls and links.
        let body = r#"{
            "message_id": 42,
            "date": 1770000000,
            "chat": {"id": -1001234567890, "type": "channel", "title": "test"},
            "rich_message": {
                "blocks": [{"type": "paragraph", "text": "hello"}]
            }
        }"#;
        let parsed: Message = serde_json::from_str(body).expect("rich response should parse");
        assert_eq!(parsed.id.0, 42);
        assert_eq!(parsed.chat.id.0, -1001234567890);
    }
}
