//! Shared post-send side effects.
//!
//! Both the fetch cycle and the outbox retry path consume a `SendOutcome` and
//! must emit the same owner notices / persistence updates. Keeping this in one
//! place prevents the two paths from drifting (the outbox path previously had
//! no `uncertain` handling, and the chat-title/language lookups were repeated).

use teloxide::prelude::*;
use tracing::warn;

use crate::db;
use crate::i18n::{self, Lang};
use crate::notify::{notify_owner_html, push_line};
use crate::push;
use crate::state::AppState;

/// Common context for owner-facing post-send effects.
pub(super) struct SendSuccessCtx<'a> {
    pub bot: &'a Bot,
    pub state: &'a AppState,
    pub chat_id: &'a str,
    pub feed_url: &'a str,
    pub entry_id: &'a str,
    pub chat_title: &'a str,
    pub lang: Lang,
}

fn uncertain_msg(lang: Lang, chat_title: &str, entry_link: Option<&str>) -> String {
    let mut msg = i18n::t(lang, "notif_send_uncertain")
        .replacen("{}", &push::escape_html(chat_title), 1);
    if let Some(el) = entry_link {
        push_line(&mut msg, lang, "label_entry", el);
    }
    msg
}

fn text_partial_msg(
    lang: Lang,
    chat_title: &str,
    delivered_tg_link: Option<&str>,
    entry_link: Option<&str>,
) -> String {
    let mut msg = i18n::t(lang, "notif_text_partial")
        .replacen("{}", &push::escape_html(chat_title), 1);
    if let Some(tg) = delivered_tg_link {
        push_line(&mut msg, lang, "label_message", tg);
    }
    if let Some(el) = entry_link {
        push_line(&mut msg, lang, "label_entry", el);
    }
    msg
}

fn media_failed_msg(
    lang: Lang,
    chat_title: &str,
    delivered_tg_link: Option<&str>,
    entry_link: Option<&str>,
) -> String {
    let mut msg = i18n::t(lang, "notif_media_failed")
        .replacen("{}", &push::escape_html(chat_title), 1);
    if let Some(tg) = delivered_tg_link {
        push_line(&mut msg, lang, "label_message", tg);
    }
    if let Some(el) = entry_link {
        push_line(&mut msg, lang, "label_entry", el);
    }
    msg
}

impl SendSuccessCtx<'_> {
    /// A1 policy: a rich message may have been delivered even though the API
    /// call reported an error. The entry is still marked sent (no outbox, no
    /// duplicate), but the owner is asked to verify the channel.
    pub(super) async fn notify_uncertain(&self, outcome: &push::SendOutcome) {
        if !outcome.uncertain {
            return;
        }
        let msg = uncertain_msg(self.lang, self.chat_title, outcome.entry_link.as_deref());
        notify_owner_html(self.bot, self.state, self.chat_id, &msg).await;
    }

    /// The first text chunk was delivered but later chunks were not. The entry
    /// is stopped rather than replayed as a whole.
    pub(super) async fn notify_text_partial(&self, outcome: &push::SendOutcome) {
        if !outcome.text_partial {
            return;
        }
        let msg = text_partial_msg(
            self.lang,
            self.chat_title,
            outcome.delivered_tg_link.as_deref(),
            outcome.entry_link.as_deref(),
        );
        notify_owner_html(self.bot, self.state, self.chat_id, &msg).await;
    }

    /// The text was delivered but the media was not (failed or expired).
    pub(super) async fn notify_media_failed(&self, outcome: &push::SendOutcome) {
        if !outcome.media_failed {
            return;
        }
        let msg = media_failed_msg(
            self.lang,
            self.chat_title,
            outcome.delivered_tg_link.as_deref(),
            outcome.entry_link.as_deref(),
        );
        notify_owner_html(self.bot, self.state, self.chat_id, &msg).await;
    }

    /// Register the delivered message for `/find` and mark the entry sent.
    /// A persistence failure after Telegram delivery is best-effort + warn:
    /// returning an error would make the caller replay an already-delivered
    /// entry.
    pub(super) async fn record_and_mark_sent(
        &self,
        outcome: &push::SendOutcome,
        mark_log: &'static str,
    ) {
        if let Some(mid) = outcome.delivered_message_id {
            db::record_sent_message(
                &self.state.db,
                self.chat_id,
                mid,
                self.feed_url,
                self.entry_id,
                outcome.entry_link.as_deref(),
            )
            .await
            .ok();
        }
        if let Err(e) = db::mark_entry_sent(
            &self.state.db,
            self.chat_id,
            self.feed_url,
            self.entry_id,
        )
        .await
        {
            warn!(feed = %self.feed_url, entry_id = %self.entry_id, error = %e, "{}", mark_log);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_msg_carries_channel_and_entry_link() {
        let msg = uncertain_msg(
            Lang::En,
            "My Channel",
            Some("https://example.com/post"),
        );
        assert!(msg.contains("Rich message delivery is uncertain"));
        assert!(msg.contains("My Channel"));
        assert!(msg.contains("\nEntry: https://example.com/post"));
    }

    #[test]
    fn text_partial_msg_uses_locale_labels() {
        let en = text_partial_msg(
            Lang::En,
            "Channel",
            Some("https://t.me/c/1/2"),
            Some("https://example.com/post"),
        );
        assert!(en.contains("Text delivery failed partway"));
        assert!(en.contains("\nMessage: https://t.me/c/1/2"));
        assert!(en.contains("\nEntry: https://example.com/post"));

        let zh = text_partial_msg(
            Lang::Zh,
            "频道",
            None,
            Some("https://example.com/post"),
        );
        assert!(zh.contains("正文分段发送失败"));
        assert!(zh.contains("\n条目：https://example.com/post"));
        assert!(!zh.contains("Message:"));
    }

    #[test]
    fn media_failed_msg_is_localized_and_escapes_the_title() {
        let msg = media_failed_msg(
            Lang::En,
            "A <Channel>",
            None,
            Some("https://example.com/post"),
        );
        assert!(msg.contains("Media send failed"));
        assert!(msg.contains("A &lt;Channel&gt;"));
        assert!(msg.contains("\nEntry: https://example.com/post"));
    }
}
