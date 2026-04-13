use zbus::names::OwnedUniqueName;

#[derive(Debug, Clone)]
pub struct Discovery {
    pub bus_label: String,
    pub vms: Vec<VmSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct VmSummary {
    pub source_label: String,
    pub source_address: Option<String>,
    pub owner: OwnedUniqueName,
    pub name: String,
    pub uuid: String,
    pub console_ids: Vec<u32>,
    pub interfaces: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InspectionReport {
    pub bus_label: String,
    pub vm: VmSummary,
    pub has_audio: bool,
    pub has_clipboard: bool,
    pub consoles: Vec<ConsoleSummary>,
    pub chardevs: Vec<ChardevSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConnectTarget {
    pub source_address: Option<String>,
    pub owner: String,
    pub vm_name: String,
    pub console_id: u32,
    pub width: u32,
    pub height: u32,
    pub console_interfaces: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConsoleSummary {
    pub id: u32,
    pub label: String,
    pub head: u32,
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub interfaces: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChardevSummary {
    pub name: String,
    pub owner: String,
    pub frontend_open: bool,
    pub echo: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClipboardAgentStatus {
    Unknown,
    GuestDisconnected,
    Connected,
}
