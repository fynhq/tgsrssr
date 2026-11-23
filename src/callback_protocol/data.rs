//! Only known, bounded callback ops parse; unknown or oversized data must fail closed before dispatch.

use super::GuardTarget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallbackOp {
    Channel,
    ChannelList,
    Language,
    Unsub,
    UnsubPage,
    UnsubAllAsk,
    UnsubAll,
    ResumeAll,
    Config,
    ConfigInterval,
    ConfigMode,
    ConfigSource,
    ConfigLinkPreview,
    ConfigCaption,
    ConfigIntervalValue,
    ConfigModeValue,
    ConfigSourceValue,
    Source,
    SourcePause,
    SourceResume,
    SourceRename,
    SourceIntervalPage,
    SourceModePage,
    SourceSourcePage,
    SourceCaption,
    SourceLinkPreview,
    SourceFilter,
    SourceFilterToggle,
    SourceFilterMode,
    SourceFilterKeywords,
    SourceIntervalValue,
    SourceModeValue,
    SourceSourceValue,
    Cookies,
    CancelOp,
    Page,
}

impl CallbackOp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Channel => "ch",
            Self::ChannelList => "chlist",
            Self::Language => "lang",
            Self::Unsub => "unsub",
            Self::UnsubPage => "unsubpg",
            Self::UnsubAllAsk => "unsuballask",
            Self::UnsubAll => "unsuball",
            Self::ResumeAll => "resumeall",
            Self::Config => "cfg",
            Self::ConfigInterval => "cfgi",
            Self::ConfigMode => "cfgm",
            Self::ConfigSource => "cfgs",
            Self::ConfigLinkPreview => "cfglp",
            Self::ConfigCaption => "cfgcp",
            Self::ConfigIntervalValue => "cfgiv",
            Self::ConfigModeValue => "cfgmv",
            Self::ConfigSourceValue => "cfgsv",
            Self::Source => "src",
            Self::SourcePause => "srcpause",
            Self::SourceResume => "srcresume",
            Self::SourceRename => "srcrename",
            Self::SourceIntervalPage => "srcfip",
            Self::SourceModePage => "srcfmp",
            Self::SourceSourcePage => "srcfsp",
            Self::SourceCaption => "srcfcp",
            Self::SourceLinkPreview => "srcflp",
            Self::SourceFilter => "srcfilter",
            Self::SourceFilterToggle => "srcftoggle",
            Self::SourceFilterMode => "srcfmode",
            Self::SourceFilterKeywords => "srcfkw",
            Self::SourceIntervalValue => "srcfi",
            Self::SourceModeValue => "srcfm",
            Self::SourceSourceValue => "srcfs",
            Self::Cookies => "ck",
            Self::CancelOp => "cancelop",
            Self::Page => "page",
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "ch" => Self::Channel,
            "chlist" => Self::ChannelList,
            "lang" => Self::Language,
            "unsub" => Self::Unsub,
            "unsubpg" => Self::UnsubPage,
            "unsuballask" => Self::UnsubAllAsk,
            "unsuball" => Self::UnsubAll,
            "resumeall" => Self::ResumeAll,
            "cfg" => Self::Config,
            "cfgi" => Self::ConfigInterval,
            "cfgm" => Self::ConfigMode,
            "cfgs" => Self::ConfigSource,
            "cfglp" => Self::ConfigLinkPreview,
            "cfgcp" => Self::ConfigCaption,
            "cfgiv" => Self::ConfigIntervalValue,
            "cfgmv" => Self::ConfigModeValue,
            "cfgsv" => Self::ConfigSourceValue,
            "src" => Self::Source,
            "srcpause" => Self::SourcePause,
            "srcresume" => Self::SourceResume,
            "srcrename" => Self::SourceRename,
            "srcfip" => Self::SourceIntervalPage,
            "srcfmp" => Self::SourceModePage,
            "srcfsp" => Self::SourceSourcePage,
            "srcfcp" => Self::SourceCaption,
            "srcflp" => Self::SourceLinkPreview,
            "srcfilter" => Self::SourceFilter,
            "srcftoggle" => Self::SourceFilterToggle,
            "srcfmode" => Self::SourceFilterMode,
            "srcfkw" => Self::SourceFilterKeywords,
            "srcfi" => Self::SourceIntervalValue,
            "srcfm" => Self::SourceModeValue,
            "srcfs" => Self::SourceSourceValue,
            "ck" => Self::Cookies,
            "cancelop" => Self::CancelOp,
            "page" => Self::Page,
            _ => return None,
        })
    }

    pub(crate) fn guard_target(self) -> GuardTarget {
        match self {
            Self::Channel
            | Self::Config
            | Self::ConfigInterval
            | Self::ConfigMode
            | Self::ConfigSource
            | Self::ConfigLinkPreview
            | Self::ConfigCaption
            | Self::ResumeAll
            | Self::ConfigIntervalValue
            | Self::ConfigModeValue
            | Self::ConfigSourceValue
            | Self::UnsubPage
            | Self::UnsubAllAsk
            | Self::UnsubAll => GuardTarget::Chat,
            Self::Source
            | Self::SourcePause
            | Self::SourceResume
            | Self::SourceRename
            | Self::SourceIntervalPage
            | Self::SourceModePage
            | Self::SourceSourcePage
            | Self::SourceCaption
            | Self::SourceLinkPreview
            | Self::SourceFilter
            | Self::SourceFilterToggle
            | Self::SourceFilterMode
            | Self::SourceFilterKeywords
            | Self::SourceIntervalValue
            | Self::SourceModeValue
            | Self::SourceSourceValue
            | Self::Unsub => GuardTarget::Sub,
            Self::ChannelList | Self::Language | Self::Cookies | Self::CancelOp | Self::Page => {
                GuardTarget::None
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CallbackArgs<'a> {
    pub id: Option<&'a str>,
    pub value: Option<&'a str>,
    pub page: usize,
    pub ch_page: usize,
    pub chlist_page: usize,
}

impl CallbackArgs<'_> {
    pub(crate) fn sub_id(&self) -> Option<i64> {
        self.id?.parse().ok()
    }
}

pub(crate) struct CallbackData<'a> {
    pub op: CallbackOp,
    pub args: CallbackArgs<'a>,
}

impl<'a> CallbackData<'a> {
    pub(crate) fn parse(raw: &'a str) -> Option<Self> {
        if raw.len() > 64 {
            return None;
        }
        let mut parts = raw.split('|');
        if parts.next() != Some("c") {
            return None;
        }
        let op = CallbackOp::parse(parts.next()?)?;
        let raw_args: Vec<&str> = parts.collect();
        if raw_args.len() > 5 {
            return None;
        }
        Some(Self {
            op,
            args: parse_args(op, &raw_args),
        })
    }
}

fn parse_args<'a>(op: CallbackOp, args: &[&'a str]) -> CallbackArgs<'a> {
    let at = |i: usize| args.get(i).copied();
    let page = |i: usize| at(i).and_then(|s| s.parse().ok()).unwrap_or(0);
    match op {
        CallbackOp::Channel => CallbackArgs {
            id: at(0),
            ch_page: page(1),
            chlist_page: page(2),
            ..CallbackArgs::default()
        },
        CallbackOp::ChannelList => CallbackArgs {
            page: page(0),
            ..CallbackArgs::default()
        },
        CallbackOp::Language | CallbackOp::Cookies => CallbackArgs {
            value: at(0),
            ..CallbackArgs::default()
        },
        CallbackOp::Unsub => CallbackArgs {
            id: at(0),
            page: page(1),
            ch_page: page(2),
            chlist_page: page(3),
            ..CallbackArgs::default()
        },
        CallbackOp::UnsubPage | CallbackOp::UnsubAllAsk | CallbackOp::UnsubAll => CallbackArgs {
            id: at(0),
            page: page(1),
            ch_page: page(2),
            chlist_page: page(3),
            ..CallbackArgs::default()
        },
        CallbackOp::ResumeAll => CallbackArgs {
            id: at(0),
            chlist_page: page(1),
            ch_page: page(2),
            ..CallbackArgs::default()
        },
        CallbackOp::Config
        | CallbackOp::ConfigInterval
        | CallbackOp::ConfigMode
        | CallbackOp::ConfigSource
        | CallbackOp::ConfigLinkPreview
        | CallbackOp::ConfigCaption
        | CallbackOp::Source
        | CallbackOp::SourcePause
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
        | CallbackOp::SourceFilterKeywords => CallbackArgs {
            id: at(0),
            ch_page: page(1),
            chlist_page: page(2),
            ..CallbackArgs::default()
        },
        CallbackOp::ConfigIntervalValue
        | CallbackOp::ConfigModeValue
        | CallbackOp::ConfigSourceValue
        | CallbackOp::SourceIntervalValue
        | CallbackOp::SourceModeValue
        | CallbackOp::SourceSourceValue => CallbackArgs {
            id: at(0),
            value: at(1),
            ch_page: page(2),
            chlist_page: page(3),
            ..CallbackArgs::default()
        },
        CallbackOp::CancelOp | CallbackOp::Page => CallbackArgs::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_parses_and_keeps_its_guard_tier() {
        for (raw, op, target) in [
            ("ch", CallbackOp::Channel, GuardTarget::Chat),
            ("chlist", CallbackOp::ChannelList, GuardTarget::None),
            ("lang", CallbackOp::Language, GuardTarget::None),
            ("unsub", CallbackOp::Unsub, GuardTarget::Sub),
            ("unsubpg", CallbackOp::UnsubPage, GuardTarget::Chat),
            ("unsuballask", CallbackOp::UnsubAllAsk, GuardTarget::Chat),
            ("unsuball", CallbackOp::UnsubAll, GuardTarget::Chat),
            ("resumeall", CallbackOp::ResumeAll, GuardTarget::Chat),
            ("cfg", CallbackOp::Config, GuardTarget::Chat),
            ("cfgi", CallbackOp::ConfigInterval, GuardTarget::Chat),
            ("cfgm", CallbackOp::ConfigMode, GuardTarget::Chat),
            ("cfgs", CallbackOp::ConfigSource, GuardTarget::Chat),
            ("cfglp", CallbackOp::ConfigLinkPreview, GuardTarget::Chat),
            ("cfgcp", CallbackOp::ConfigCaption, GuardTarget::Chat),
            ("cfgiv", CallbackOp::ConfigIntervalValue, GuardTarget::Chat),
            ("cfgmv", CallbackOp::ConfigModeValue, GuardTarget::Chat),
            ("cfgsv", CallbackOp::ConfigSourceValue, GuardTarget::Chat),
            ("src", CallbackOp::Source, GuardTarget::Sub),
            ("srcpause", CallbackOp::SourcePause, GuardTarget::Sub),
            ("srcresume", CallbackOp::SourceResume, GuardTarget::Sub),
            ("srcrename", CallbackOp::SourceRename, GuardTarget::Sub),
            ("srcfip", CallbackOp::SourceIntervalPage, GuardTarget::Sub),
            ("srcfmp", CallbackOp::SourceModePage, GuardTarget::Sub),
            ("srcfsp", CallbackOp::SourceSourcePage, GuardTarget::Sub),
            ("srcfcp", CallbackOp::SourceCaption, GuardTarget::Sub),
            ("srcflp", CallbackOp::SourceLinkPreview, GuardTarget::Sub),
            ("srcfilter", CallbackOp::SourceFilter, GuardTarget::Sub),
            (
                "srcftoggle",
                CallbackOp::SourceFilterToggle,
                GuardTarget::Sub,
            ),
            ("srcfmode", CallbackOp::SourceFilterMode, GuardTarget::Sub),
            ("srcfkw", CallbackOp::SourceFilterKeywords, GuardTarget::Sub),
            ("srcfi", CallbackOp::SourceIntervalValue, GuardTarget::Sub),
            ("srcfm", CallbackOp::SourceModeValue, GuardTarget::Sub),
            ("srcfs", CallbackOp::SourceSourceValue, GuardTarget::Sub),
            ("ck", CallbackOp::Cookies, GuardTarget::None),
            ("cancelop", CallbackOp::CancelOp, GuardTarget::None),
            ("page", CallbackOp::Page, GuardTarget::None),
        ] {
            let full = format!("c|{raw}");
            let parsed = CallbackData::parse(&full).expect(&full);
            assert_eq!(parsed.op, op);
            assert_eq!(op.as_str(), raw, "{raw}");
            assert_eq!(parsed.op.guard_target(), target, "{raw}");
        }
    }

    #[test]
    fn arguments_are_parsed_into_named_fields() {
        let parsed = CallbackData::parse("c|ch|-100123|2|1").expect("valid");
        assert_eq!(parsed.op, CallbackOp::Channel);
        assert_eq!(parsed.args.id, Some("-100123"));
        assert_eq!(parsed.args.ch_page, 2);
        assert_eq!(parsed.args.chlist_page, 1);

        let parsed = CallbackData::parse("c|srcfm|7|title_summary|3|4").expect("valid");
        assert_eq!(parsed.op, CallbackOp::SourceModeValue);
        assert_eq!(parsed.args.sub_id(), Some(7));
        assert_eq!(parsed.args.value, Some("title_summary"));
        assert_eq!(parsed.args.ch_page, 3);
        assert_eq!(parsed.args.chlist_page, 4);
    }

    #[test]
    fn unsub_all_shares_the_page_order_of_unsub_all_ask() {
        let ask = CallbackData::parse("c|unsuballask|-100123|2|3|4").expect("valid");
        assert_eq!(ask.args.page, 2);
        assert_eq!(ask.args.ch_page, 3);
        assert_eq!(ask.args.chlist_page, 4);

        let all = CallbackData::parse("c|unsuball|-100123|2|3|4").expect("valid");
        assert_eq!(all.args.page, 2);
        assert_eq!(all.args.ch_page, 3);
        assert_eq!(all.args.chlist_page, 4);
    }

    #[test]
    fn unknown_prefix_unknown_op_and_oversized_data_are_rejected() {
        assert!(CallbackData::parse("x|ch").is_none());
        assert!(CallbackData::parse("c|nope").is_none());
        assert!(CallbackData::parse("c").is_none());
        assert!(CallbackData::parse("c|").is_none());
        let too_long = format!("c|{}", "x".repeat(63));
        assert!(CallbackData::parse(&too_long).is_none());
        assert!(CallbackData::parse("c|ch|1|2|3|4|5|6").is_none());
    }
}
