//! Single source for callback wire-format construction.
//!
//! Panels build `c|<op>|...` strings here instead of scattering `format!`
//! literals. The format itself is unchanged, so callbacks already present in
//! Telegram chats keep working.

pub(crate) use super::data::CallbackOp;

pub(crate) fn channel(chat_id: &str, ch_page: usize, chlist_page: usize) -> String {
    format!("c|ch|{chat_id}|{ch_page}|{chlist_page}")
}

pub(crate) fn channel_list(page: usize) -> String {
    format!("c|chlist|{page}")
}

pub(crate) fn language(code: &str) -> String {
    format!("c|lang|{code}")
}

pub(crate) fn unsub(sub_id: i64, page: usize, ch_page: usize, chlist_page: usize) -> String {
    format!("c|unsub|{sub_id}|{page}|{ch_page}|{chlist_page}")
}

pub(crate) fn unsub_page(chat_id: &str, page: usize, ch_page: usize, chlist_page: usize) -> String {
    format!("c|unsubpg|{chat_id}|{page}|{ch_page}|{chlist_page}")
}

pub(crate) fn unsub_all_ask(
    chat_id: &str,
    page: usize,
    ch_page: usize,
    chlist_page: usize,
) -> String {
    format!("c|unsuballask|{chat_id}|{page}|{ch_page}|{chlist_page}")
}

pub(crate) fn unsub_all(chat_id: &str, ch_page: usize, page: usize, chlist_page: usize) -> String {
    format!("c|unsuball|{chat_id}|{ch_page}|{page}|{chlist_page}")
}

pub(crate) fn resume_all(chat_id: &str, chlist_page: usize, ch_page: usize) -> String {
    format!("c|resumeall|{chat_id}|{chlist_page}|{ch_page}")
}

pub(crate) fn config(chat_id: &str, ch_page: usize, chlist_page: usize) -> String {
    format!("c|cfg|{chat_id}|{ch_page}|{chlist_page}")
}

pub(crate) fn config_page(
    op: CallbackOp,
    chat_id: &str,
    ch_page: usize,
    chlist_page: usize,
) -> String {
    debug_assert!(matches!(
        op,
        CallbackOp::ConfigInterval
            | CallbackOp::ConfigMode
            | CallbackOp::ConfigSource
            | CallbackOp::ConfigLinkPreview
            | CallbackOp::ConfigCaption
    ));
    format!("c|{}|{chat_id}|{ch_page}|{chlist_page}", op.as_str())
}

pub(crate) fn config_value(
    op: CallbackOp,
    chat_id: &str,
    value: &str,
    ch_page: usize,
    chlist_page: usize,
) -> String {
    debug_assert!(matches!(
        op,
        CallbackOp::ConfigIntervalValue
            | CallbackOp::ConfigModeValue
            | CallbackOp::ConfigSourceValue
    ));
    format!(
        "c|{}|{chat_id}|{value}|{ch_page}|{chlist_page}",
        op.as_str()
    )
}

pub(crate) fn source(sub_id: i64, ch_page: usize, chlist_page: usize) -> String {
    format!("c|src|{sub_id}|{ch_page}|{chlist_page}")
}

pub(crate) fn source_action(
    op: CallbackOp,
    sub_id: i64,
    ch_page: usize,
    chlist_page: usize,
) -> String {
    debug_assert!(matches!(
        op,
        CallbackOp::SourcePause
            | CallbackOp::SourceResume
            | CallbackOp::SourceRename
            | CallbackOp::SourceIntervalPage
            | CallbackOp::SourceModePage
            | CallbackOp::SourceSourcePage
            | CallbackOp::SourceCaption
            | CallbackOp::SourceLinkPreview
            | CallbackOp::SourceFilter
            | CallbackOp::SourceFilterToggle
            | CallbackOp::SourceFilterMode
            | CallbackOp::SourceFilterKeywords
    ));
    format!("c|{}|{sub_id}|{ch_page}|{chlist_page}", op.as_str())
}

pub(crate) fn source_value(
    op: CallbackOp,
    sub_id: i64,
    value: &str,
    ch_page: usize,
    chlist_page: usize,
) -> String {
    debug_assert!(matches!(
        op,
        CallbackOp::SourceIntervalValue
            | CallbackOp::SourceModeValue
            | CallbackOp::SourceSourceValue
    ));
    format!("c|{}|{sub_id}|{value}|{ch_page}|{chlist_page}", op.as_str())
}

pub(crate) fn cookies(platform: &str) -> String {
    format!("c|ck|{platform}")
}

/// Encode a config/source value choice (option-page rows and "inherit").
pub(crate) fn value_callback(
    op: CallbackOp,
    id_seg: &str,
    value: &str,
    ch_page: usize,
    chlist_page: usize,
) -> String {
    match op {
        CallbackOp::ConfigIntervalValue
        | CallbackOp::ConfigModeValue
        | CallbackOp::ConfigSourceValue => config_value(op, id_seg, value, ch_page, chlist_page),
        CallbackOp::SourceIntervalValue
        | CallbackOp::SourceModeValue
        | CallbackOp::SourceSourceValue => source_value(
            op,
            id_seg.parse().unwrap_or_default(),
            value,
            ch_page,
            chlist_page,
        ),
        _ => unreachable!("value_callback op: {op:?}"),
    }
}

pub(crate) fn cancelop() -> &'static str {
    "c|cancelop"
}

pub(crate) fn page() -> &'static str {
    "c|page"
}

/// Prefix for the generic option-page renderer (`c|<op>`); values are appended
/// by `OptionPageSpec`.
pub(crate) fn op_prefix(op: CallbackOp) -> String {
    format!("c|{}", op.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::data::CallbackData;

    #[test]
    fn constructors_round_trip_through_the_parser() {
        let cases = [
            (CallbackOp::Channel, channel("-1001", 2, 3)),
            (CallbackOp::ChannelList, channel_list(4)),
            (CallbackOp::Language, language("zh")),
            (CallbackOp::Unsub, unsub(7, 1, 2, 3)),
            (CallbackOp::UnsubPage, unsub_page("-1001", 1, 2, 3)),
            (CallbackOp::UnsubAllAsk, unsub_all_ask("-1001", 1, 2, 3)),
            (CallbackOp::UnsubAll, unsub_all("-1001", 2, 1, 3)),
            (CallbackOp::ResumeAll, resume_all("-1001", 3, 2)),
            (CallbackOp::Config, config("-1001", 2, 3)),
            (
                CallbackOp::ConfigInterval,
                config_page(CallbackOp::ConfigInterval, "-1001", 2, 3),
            ),
            (
                CallbackOp::ConfigModeValue,
                config_value(CallbackOp::ConfigModeValue, "-1001", "title_summary", 2, 3),
            ),
            (CallbackOp::Source, source(7, 2, 3)),
            (
                CallbackOp::SourcePause,
                source_action(CallbackOp::SourcePause, 7, 2, 3),
            ),
            (
                CallbackOp::SourceIntervalValue,
                source_value(CallbackOp::SourceIntervalValue, 7, "inherit", 2, 3),
            ),
            (CallbackOp::Cookies, cookies("weibo")),
            (CallbackOp::CancelOp, "c|cancelop".to_string()),
            (CallbackOp::Page, "c|page".to_string()),
        ];
        for (op, raw) in cases {
            let parsed = CallbackData::parse(&raw).expect(&raw);
            assert_eq!(parsed.op, op, "{raw}");
        }
    }

    #[test]
    fn op_prefix_matches_the_constructor_op() {
        assert_eq!(op_prefix(CallbackOp::ConfigIntervalValue), "c|cfgiv");
        assert_eq!(op_prefix(CallbackOp::SourceIntervalValue), "c|srcfi");
    }
}
