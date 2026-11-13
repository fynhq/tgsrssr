//! Typed callback-data parsing and the single op → ownership-guard mapping.
//!
//! The wire format stays `c|<op>|...` so panels already in Telegram chats keep
//! working. Only known ops parse; unknown or oversized data is rejected before
//! dispatch (fail closed). The handler still consumes the raw segments for
//! page/value parsing; this module owns the protocol identity and guard tier.

use super::GuardTarget;

/// All callback operations the bot understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallbackOp {
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
    pub(super) fn parse(raw: &str) -> Option<Self> {
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

    /// Ownership-guard tier for this op. This is the only mapping; the
    /// dispatcher no longer maintains a parallel op list.
    pub(super) fn guard_target(self) -> GuardTarget {
        match self {
            // chat level: args[0] is the chat_id
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
            // sub level: args[0] is the sub_id
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
            // explicitly no target channel involved
            Self::ChannelList | Self::Language | Self::Cookies | Self::CancelOp | Self::Page => {
                GuardTarget::None
            }
        }
    }
}

/// Parsed callback: typed op plus the raw argument segments after it.
pub(super) struct CallbackData<'a> {
    pub op: CallbackOp,
    pub args: Vec<&'a str>,
}

impl<'a> CallbackData<'a> {
    /// Parse `c|<op>[|arg...]`. Telegram caps callback data at 64 bytes; ops
    /// with at most five arguments exist, so anything larger is malformed.
    pub(super) fn parse(raw: &'a str) -> Option<Self> {
        if raw.len() > 64 {
            return None;
        }
        let mut parts = raw.split('|');
        if parts.next() != Some("c") {
            return None;
        }
        let op = CallbackOp::parse(parts.next()?)?;
        let args: Vec<&str> = parts.collect();
        if args.len() > 5 {
            return None;
        }
        Some(Self { op, args })
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
            let raw = format!("c|{raw}");
            let parsed = CallbackData::parse(&raw).expect(&raw);
            assert_eq!(parsed.op, op);
            assert_eq!(parsed.op.guard_target(), target, "{raw}");
        }
    }

    #[test]
    fn arguments_are_collected_after_the_op() {
        let parsed = CallbackData::parse("c|ch|-100123|2|1").expect("valid");
        assert_eq!(parsed.op, CallbackOp::Channel);
        assert_eq!(parsed.args, vec!["-100123", "2", "1"]);
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
