use std::collections::HashMap;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct EntryMeta {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prerendered_html: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<PollData>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retweet_images: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PollData {
    pub question: String,
    pub options: Vec<String>,
    pub is_multi_select: bool,
}

impl EntryMeta {
    pub fn is_empty(&self) -> bool {
        !self.prerendered_html && self.poll.is_none() && self.retweet_images.is_empty()
    }
}

pub type MetaMap = HashMap<String, EntryMeta>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_full_meta() {
        let meta = EntryMeta {
            prerendered_html: true,
            poll: Some(PollData {
                question: "Have you ever moved house?".to_string(),
                options: vec!["A: Never".to_string(), "B: Yes".to_string()],
                is_multi_select: false,
            }),
            retweet_images: vec!["https://wx2.sinaimg.cn/large/abc.jpg".to_string()],
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: EntryMeta = serde_json::from_str(&json).unwrap();
        assert!(back.prerendered_html);
        assert_eq!(
            back.poll.as_ref().unwrap().question,
            "Have you ever moved house?"
        );
        assert_eq!(back.poll.as_ref().unwrap().options.len(), 2);
        assert_eq!(back.retweet_images.len(), 1);
    }

    #[test]
    fn serde_roundtrip_default_meta() {
        let meta = EntryMeta::default();
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(json, "{}");
        let back: EntryMeta = serde_json::from_str(&json).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn serde_empty_string_deserializes_to_none() {
        let back: EntryMeta = serde_json::from_str("{}").unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn is_empty_logic() {
        assert!(EntryMeta::default().is_empty());
        assert!(!EntryMeta {
            prerendered_html: true,
            ..Default::default()
        }
        .is_empty());
        assert!(!EntryMeta {
            retweet_images: vec!["x".to_string()],
            ..Default::default()
        }
        .is_empty());
        assert!(!EntryMeta {
            poll: Some(PollData {
                question: "q".into(),
                options: vec!["a".into(), "b".into()],
                is_multi_select: false
            }),
            ..Default::default()
        }
        .is_empty());
    }
}
