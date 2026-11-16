use teloxide::prelude::*;
use teloxide::types::ChatId;

use crate::i18n::{self, Lang};

pub async fn report(bot: &Bot, chat: ChatId, e: &anyhow::Error, lang: Lang) {
    tracing::warn!(chat = %chat, error = %e, "command error reported to user");
    let text = i18n::t(lang, "error_page").replacen("{}", &format!("{:#}", e), 1);
    if bot.send_message(chat, text).await.is_err() {
        tracing::warn!(chat = %chat, "failed to deliver error message to user");
    }
}

#[macro_export]
macro_rules! try_or_reply {
    ($bot:expr, $chat:expr, $lang:expr, $expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                $crate::errors::report($bot, $chat, &e, $lang).await;
                return Ok(());
            }
        }
    };
}
