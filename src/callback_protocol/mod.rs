//! Callback payload protocol shared by the UI and the callback dispatcher.

pub(crate) mod data;
pub(crate) mod wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardTarget {
    Chat,
    Sub,
    None,
}
