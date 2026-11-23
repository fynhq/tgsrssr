use teloxide::prelude::*;
use tracing::{error, warn};

use crate::db::{self, Db};
use crate::i18n::{self, Lang};
use crate::notify::{
    notify_download_failed, notify_oversized_videos, notify_owner_html, push_line,
};
use crate::push;
use crate::state::AppState;

pub(super) struct SendSuccessCtx<'a> {
    pub bot: &'a Bot,
    pub state: &'a AppState,
    pub chat_id: &'a str,
    pub feed_url: &'a str,
    pub entry_id: &'a str,
    pub chat_title: &'a str,
    pub lang: Lang,
}

pub(super) struct SendSuccessExtras<'a> {
    pub owner_id: Option<i64>,
    pub feed_title: &'a str,
    pub entry_link: &'a str,
    pub download_warnings: &'a [String],
    pub oversized: &'a [crate::media::OversizedVideo],
    pub notify_oversized: bool,
    pub dl_retry_note: &'a str,
    pub outbox_row_id: Option<i64>,
}

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
    pub(super) async fn notify_uncertain(&self, outcome: &push::SendOutcome) {
        if !outcome.uncertain {
            return;
        }
        let msg = uncertain_msg(self.lang, self.chat_title, outcome.entry_link.as_deref());
        notify_owner_html(self.bot, self.state, self.chat_id, &msg).await;
    }

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

    pub(super) async fn record_and_mark_sent(
        &self,
        outcome: &push::SendOutcome,
        mark_log: &'static str,
    ) -> bool {
        record_and_mark_sent(
            &self.state.db,
            self.chat_id,
            self.feed_url,
            self.entry_id,
            outcome,
            mark_log,
        )
        .await
    }
}

async fn record_and_mark_sent(
    db_pool: &Db,
    chat_id: &str,
    feed_url: &str,
    entry_id: &str,
    outcome: &push::SendOutcome,
    mark_log: &'static str,
) -> bool {
    if let Some(mid) = outcome.delivered_message_id {
        db::record_sent_message(
            db_pool,
            chat_id,
            mid,
            feed_url,
            entry_id,
            outcome.entry_link.as_deref(),
        )
        .await
        .ok();
    }
    match db::mark_entry_sent(db_pool, chat_id, feed_url, entry_id).await {
        Ok(()) => true,
        Err(e) => {
            error!(
                feed = %feed_url,
                entry_id = %entry_id,
                error = %e,
                "{}",
                mark_log
            );
            false
        }
    }
}

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
                notify_oversized_videos(
                    ctx.bot,
                    ctx.lang,
                    extras.owner_id,
                    ctx.chat_title,
                    extras.entry_link,
                    outcome.delivered_tg_link.as_deref(),
                    outcome.text_only,
                    extras.oversized,
                )
                .await;
            }
            SendSuccessEffect::RecordAndMarkSent => {
                // Keep the outbox row when the dedup mark fails: a bounded retry beats an
                // untracked entry the next fetch cycle could replay indefinitely.
                if !ctx.record_and_mark_sent(outcome, mark_log).await {
                    break;
                }
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

    async fn test_pool() -> Db {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn mark_failure_is_reported_to_the_caller() {
        let db = test_pool().await;
        sqlx::query("DROP TABLE sent_entries")
            .execute(&db)
            .await
            .unwrap();
        let marked = record_and_mark_sent(
            &db,
            "1",
            "https://example.com/feed",
            "e1",
            &outcome(),
            "test mark",
        )
        .await;
        assert!(!marked);
    }
}
