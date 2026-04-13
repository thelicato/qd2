mod discovery;
mod inspection;
mod selection;
mod types;
mod warnings;

pub use discovery::discover;
pub use inspection::{inspect, resolve_connect_target};
pub use types::{
    ChardevSummary, ConnectTarget, ConsoleSummary, Discovery, InspectionReport, VmSummary,
};

pub(crate) use discovery::connect;
pub(crate) use types::ClipboardAgentStatus;
pub(crate) use warnings::{
    clipboard_agent_status, disconnected_clipboard_agent_warning, missing_audio_warning,
    missing_clipboard_warning, suggested_inspection_warnings, unverifiable_clipboard_agent_note,
};
