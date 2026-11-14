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
use crate::notify::{notify_download_failed, notify_oversized_video, notify_owner_html, push_line};
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

/// Inputs that only the fetch path has. The outbox path passes empty slices,
/// `notify_oversized = false`, and `outbox_row_id = Some(..)`.
pub(super) struct SendSuccessExtras<'a> {
    pub owner_id: Option<i64>,
    pub feed_title: &'a str,
    pub entry_link: &'a str,
    pub download_warnings: &'a [String],
    pub oversized: &'a [crate::media::OversizedVideo],
    pub notify_oversized: bool,
    pub dl_retry_note: &'a str,
    /// Some(row id) on the outbox retry path; None on the fetch path.
    pub outbox_row_id: Option<i64>,
}

/// One post-send step. The plan is data so both send paths can be asserted to
/// emit the same sequence; the executor performs the I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SendSuccessEffect {
    NotifyUncertain,
    NotifyTextPartial,
    ClearSendFailure,
    NotifyDownloadFailed,
    NotifyMediaFailed,
    NotifyOversizedVideos,
    RecordAndMarkSent,
    DeleteOutboxRow,
    ClearOutboxMediaLost,
}

/// Ordered post-send effects. The fetch path's historical order is the
/// golden order; the outbox path uses the same plan plus its two row-only
/// steps. Keeping this pure prevents the two paths from drifting again.
pub(super) fn success_effect_plan(
    outcome: &push::SendOutcome,
    extras: &SendSuccessExtras<'_>,
) -> Vec<SendSuccessEffect> {
    let mut plan = Vec::with_capacity(9);
    if outcome.uncertain {
        plan.push(SendSuccessEffect::NotifyUncertain);
    }
    if outcome.text_partial {
        plan.push(SendSuccessEffect::NotifyTextPartial);
    }
    plan.push(SendSuccessEffect::ClearSendFailure);
    if !extras.download_warnings.is_empty() {
        plan.push(SendSuccessEffect::NotifyDownloadFailed);
    }
    if outcome.media_failed {
        plan.push(SendSuccessEffect::NotifyMediaFailed);
    }
    if extras.notify_oversized && !extras.oversized.is_empty() {
        plan.push(SendSuccessEffect::NotifyOversizedVideos);
    }
    plan.push(SendSuccessEffect::RecordAndMarkSent);
    if extras.outbox_row_id.is_some() {
        plan.push(SendSuccessEffect::DeleteOutboxRow);
        plan.push(SendSuccessEffect::ClearOutboxMediaLost);
    }
    plan
}

fn uncertain_msg(lang: Lang, chat_title: &str, entry_link: Option<&str>) -> String {
    let mut msg =
        i18n::t(lang, "notif_send_uncertain").replacen("{}", &push::escape_html(chat_title), 1);
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
    let mut msg =
        i18n::t(lang, "notif_text_partial").replacen("{}", &push::escape_html(chat_title), 1);
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
    let mut msg =
        i18n::t(lang, "notif_media_failed").replacen("{}", &push::escape_html(chat_title), 1);
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
        if let Err(e) =
            db::mark_entry_sent(&self.state.db, self.chat_id, self.feed_url, self.entry_id).await
        {
            warn!(feed = %self.feed_url, entry_id = %self.entry_id, error = %e, "{}", mark_log);
        }
    }
}

/// Execute the ordered post-send effects. This is the single success path for
/// both the fetch cycle and the outbox retry; `mark_log` keeps each caller's
/// existing diagnostic wording.
pub(super) async fn apply_send_success(
    ctx: &SendSuccessCtx<'_>,
    outcome: &push::SendOutcome,
    extras: &SendSuccessExtras<'_>,
    mark_log: &'static str,
) {
    for effect in success_effect_plan(outcome, extras) {
        match effect {
            SendSuccessEffect::NotifyUncertain => ctx.notify_uncertain(outcome).await,
            SendSuccessEffect::NotifyTextPartial => ctx.notify_text_partial(outcome).await,
            SendSuccessEffect::ClearSendFailure => {
                super::clear_send_failure(ctx.state, ctx.chat_id, ctx.feed_url).await;
            }
            SendSuccessEffect::NotifyDownloadFailed => {
                notify_download_failed(
                    ctx.bot,
                    ctx.lang,
                    extras.owner_id,
                    extras.feed_title,
                    extras.download_warnings,
                    extras.dl_retry_note,
                    outcome.delivered_tg_link.as_deref(),
                    extras.entry_link,
                )
                .await;
            }
            SendSuccessEffect::NotifyMediaFailed => ctx.notify_media_failed(outcome).await,
            SendSuccessEffect::NotifyOversizedVideos => {
                for ov in extras.oversized {
                    notify_oversized_video(
                        ctx.bot,
                        ctx.lang,
                        extras.owner_id,
                        ctx.chat_title,
                        extras.entry_link,
                        outcome.delivered_tg_link.as_deref(),
                        outcome.text_only,
                        ov,
                    )
                    .await;
                }
            }
            SendSuccessEffect::RecordAndMarkSent => {
                ctx.record_and_mark_sent(outcome, mark_log).await;
            }
            SendSuccessEffect::DeleteOutboxRow => {
                let Some(row_id) = extras.outbox_row_id else {
                    continue;
                };
                if let Err(e) = db::outbox_delete(&ctx.state.db, row_id).await {
                    warn!(entry_id = %ctx.entry_id, error = %e, "outbox: failed to delete entry");
                }
            }
            SendSuccessEffect::ClearOutboxMediaLost => {
                let Some(row_id) = extras.outbox_row_id else {
                    continue;
                };
                db::delete_setting(&ctx.state.db, &db::setting_key::outbox_media_lost(row_id))
                    .await
                    .ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_msg_carries_channel_and_entry_link() {
        let msg = uncertain_msg(Lang::En, "My Channel", Some("https://example.com/post"));
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
            "\u{9891}\u{9053}",
            None,
            Some("https://example.com/post"),
        );
        // Unicode escapes keep literal CJK out of the public source tree while
        // still asserting the exact Chinese locale output.
        assert!(zh.contains("\u{6b63}\u{6587}\u{5206}\u{6bb5}\u{53d1}\u{9001}\u{5931}\u{8d25}"));
        assert!(zh.contains("\n\u{6761}\u{76ee}\u{ff1a}https://example.com/post"));
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

    fn outcome() -> push::SendOutcome {
        push::SendOutcome {
            media_failed: false,
            entry_link: None,
            delivered_tg_link: None,
            delivered_message_id: None,
            text_only: false,
            uncertain: false,
            text_partial: false,
        }
    }

    fn extras(outbox_row_id: Option<i64>) -> SendSuccessExtras<'static> {
        SendSuccessExtras {
            owner_id: Some(1),
            feed_title: "Feed",
            entry_link: "https://example.com/post",
            download_warnings: &[],
            oversized: &[],
            notify_oversized: false,
            dl_retry_note: "",
            outbox_row_id,
        }
    }

    #[test]
    fn plan_orders_fetch_notices_before_persistence() {
        let mut outcome = outcome();
        outcome.uncertain = true;
        outcome.text_partial = true;
        outcome.media_failed = true;
        assert_eq!(
            success_effect_plan(&outcome, &extras(None)),
            vec![
                SendSuccessEffect::NotifyUncertain,
                SendSuccessEffect::NotifyTextPartial,
                SendSuccessEffect::ClearSendFailure,
                SendSuccessEffect::NotifyMediaFailed,
                SendSuccessEffect::RecordAndMarkSent,
            ]
        );
    }

    #[test]
    fn plan_adds_download_and_oversized_notices_only_when_present() {
        let outcome = outcome();
        let warnings = vec!["video failed".to_string()];
        let oversized = vec![crate::media::OversizedVideo { size: Some(1) }];
        let mut with_extras = extras(None);
        with_extras.download_warnings = &warnings;
        with_extras.oversized = &oversized;
        with_extras.notify_oversized = true;
        assert_eq!(
            success_effect_plan(&outcome, &with_extras),
            vec![
                SendSuccessEffect::ClearSendFailure,
                SendSuccessEffect::NotifyDownloadFailed,
                SendSuccessEffect::NotifyOversizedVideos,
                SendSuccessEffect::RecordAndMarkSent,
            ]
        );

        let mut empty_but_enabled = extras(None);
        empty_but_enabled.notify_oversized = true;
        assert_eq!(
            success_effect_plan(&outcome, &empty_but_enabled),
            vec![
                SendSuccessEffect::ClearSendFailure,
                SendSuccessEffect::RecordAndMarkSent,
            ]
        );
    }

    #[test]
    fn plan_outbox_success_appends_row_cleanup() {
        assert_eq!(
            success_effect_plan(&outcome(), &extras(Some(42))),
            vec![
                SendSuccessEffect::ClearSendFailure,
                SendSuccessEffect::RecordAndMarkSent,
                SendSuccessEffect::DeleteOutboxRow,
                SendSuccessEffect::ClearOutboxMediaLost,
            ]
        );
    }

    #[test]
    fn plan_without_notices_still_records_and_marks() {
        assert_eq!(
            success_effect_plan(&outcome(), &extras(None)),
            vec![
                SendSuccessEffect::ClearSendFailure,
                SendSuccessEffect::RecordAndMarkSent,
            ]
        );
    }
}
