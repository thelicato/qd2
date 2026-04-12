use std::{
    collections::BTreeSet,
    convert::TryFrom,
    error::Error as StdError,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use qemu_display::{ConsoleProxy, Display};
use zbus::{
    Connection, fdo,
    names::{OwnedUniqueName, WellKnownName},
    zvariant::OwnedObjectPath,
};

use crate::diagnostics;

const DISPLAY_ROOT_PATH: &str = "/org/qemu/Display1";
const LIBVIRT_DBUS_DIRS: &[&str] = &["/run/libvirt/qemu/dbus", "/var/run/libvirt/qemu/dbus"];

#[zbus::proxy(
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/VM",
    interface = "org.qemu.Display1.VM"
)]
trait InspectVm {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> zbus::Result<String>;

    #[zbus(property, name = "ConsoleIDs")]
    fn console_ids(&self) -> zbus::Result<Vec<u32>>;

    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<String>>;
}

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
    pub vm_uuid: String,
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

#[derive(Debug, Clone)]
struct DiscoveryTarget {
    label: String,
    address: Option<String>,
}

pub async fn discover(address: Option<&str>) -> Result<Discovery> {
    match address {
        Some(address) => {
            let target = DiscoveryTarget::explicit(address);
            let vms = discover_target(&target).await?;

            Ok(Discovery {
                bus_label: describe_scope(Some(address)),
                vms,
                warnings: Vec::new(),
            })
        }
        None => auto_discover().await,
    }
}

pub async fn inspect(address: Option<&str>, selector: Option<&str>) -> Result<InspectionReport> {
    let discovery = discover(address).await?;
    let vm = select_vm(&discovery, selector)?;
    diagnostics::verbose(format!(
        "inspecting VM `{}` on {}",
        vm.name, vm.source_label
    ));
    let connection = connect(vm.source_address.as_deref()).await?;
    let display = Display::new(&connection, Some(vm.owner.clone()))
        .await
        .with_context(|| {
            format!(
                "failed to connect to QEMU display owner {} on {}",
                vm.owner, vm.source_label
            )
        })?;
    let managed_objects = managed_objects(&connection, &vm.owner).await?;

    let mut consoles = Vec::new();

    for id in &vm.console_ids {
        let console_proxy = ConsoleProxy::builder(&connection)
            .destination(vm.owner.clone())?
            .path(format!("/org/qemu/Display1/Console_{id}"))?
            .build()
            .await
            .with_context(|| format!("failed to open console {id} on {}", vm.owner))?;

        let mut interfaces = interfaces_for_console(&managed_objects, *id)?;
        interfaces.sort();

        consoles.push(ConsoleSummary {
            id: *id,
            label: console_proxy.label().await?,
            head: console_proxy.head().await?,
            kind: console_proxy.type_().await?,
            width: console_proxy.width().await?,
            height: console_proxy.height().await?,
            interfaces,
        });
    }

    let mut chardevs = Vec::new();
    for chardev in display.chardevs().await {
        chardevs.push(ChardevSummary {
            name: chardev.proxy.name().await?,
            owner: chardev.proxy.owner().await?,
            frontend_open: chardev.proxy.fe_opened().await?,
            echo: chardev.proxy.echo().await?,
        });
    }
    chardevs.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.owner.cmp(&right.owner))
    });

    let mut report = InspectionReport {
        bus_label: vm.source_label.clone(),
        vm,
        has_audio: display.audio().await?.is_some(),
        has_clipboard: display.clipboard().await?.is_some(),
        consoles,
        chardevs,
        warnings: discovery.warnings,
    };
    report
        .warnings
        .extend(suggested_inspection_warnings(&report));

    Ok(report)
}

pub async fn resolve_connect_target(
    address: Option<&str>,
    selector: Option<&str>,
    console_id: Option<u32>,
) -> Result<ConnectTarget> {
    let report = inspect(address, selector).await?;
    let console = select_console(&report, console_id)?;

    Ok(ConnectTarget {
        source_address: report.vm.source_address.clone(),
        owner: report.vm.owner.as_str().to_owned(),
        vm_name: report.vm.name.clone(),
        vm_uuid: report.vm.uuid.clone(),
        console_id: console.id,
        width: console.width,
        height: console.height,
        console_interfaces: console.interfaces.clone(),
        warnings: report.warnings,
    })
}

pub(crate) async fn connect(address: Option<&str>) -> Result<Connection> {
    match address {
        Some(address) => {
            let builder = zbus::connection::Builder::address(address)
                .map_err(|error| explain_address_connect_error(address, error))?;

            builder
                .build()
                .await
                .map_err(|error| explain_address_connect_error(address, error))
        }
        None => Connection::session()
            .await
            .context("failed to connect to the session D-Bus"),
    }
}

async fn auto_discover() -> Result<Discovery> {
    let mut vms = Vec::new();
    let (targets, mut warnings) = discovery_targets();

    for target in targets {
        match discover_target(&target).await {
            Ok(mut discovered) => vms.append(&mut discovered),
            Err(error) => warnings.push(format!("{}: {error}", target.label)),
        }
    }

    deduplicate_vms(&mut vms);
    sort_vms(&mut vms);

    Ok(Discovery {
        bus_label: describe_scope(None),
        vms,
        warnings,
    })
}

async fn discover_target(target: &DiscoveryTarget) -> Result<Vec<VmSummary>> {
    diagnostics::verbose(format!("discovering QEMU VMs on {}", target.label));
    let connection = connect(target.address.as_deref()).await?;
    let mut vms = discover_vms(&connection, target).await?;
    sort_vms(&mut vms);
    Ok(vms)
}

async fn discover_vms(connection: &Connection, target: &DiscoveryTarget) -> Result<Vec<VmSummary>> {
    let owners = match fdo::DBusProxy::new(connection)
        .await?
        .list_queued_owners(WellKnownName::from_str_unchecked("org.qemu"))
        .await
    {
        Ok(owners) => owners,
        Err(fdo::Error::NameHasNoOwner(_)) => Vec::new(),
        Err(error) => return Err(error.into()),
    };

    let mut vms = Vec::new();

    for owner in owners {
        let proxy = InspectVmProxy::builder(connection)
            .destination(owner.clone())?
            .build()
            .await
            .with_context(|| format!("failed to inspect VM owned by {owner}"))?;

        let mut interfaces = proxy.interfaces().await?;
        interfaces.sort();

        let mut console_ids = proxy.console_ids().await?.into_iter().collect::<Vec<_>>();
        console_ids.sort_unstable();

        vms.push(VmSummary {
            source_label: target.label.clone(),
            source_address: target.address.clone(),
            owner,
            name: proxy.name().await?,
            uuid: proxy.uuid().await?,
            console_ids,
            interfaces,
        });
    }

    Ok(vms)
}

async fn managed_objects(
    connection: &Connection,
    owner: &OwnedUniqueName,
) -> Result<fdo::ManagedObjects> {
    let proxy = fdo::ObjectManagerProxy::builder(connection)
        .destination(owner.clone())?
        .path(DISPLAY_ROOT_PATH)?
        .build()
        .await
        .with_context(|| format!("failed to open object manager for {}", owner))?;

    proxy
        .get_managed_objects()
        .await
        .with_context(|| format!("failed to read managed objects for {}", owner))
}

fn interfaces_for_console(
    managed_objects: &fdo::ManagedObjects,
    console_id: u32,
) -> Result<Vec<String>> {
    let path = OwnedObjectPath::try_from(format!("/org/qemu/Display1/Console_{console_id}"))?;

    Ok(managed_objects
        .get(&path)
        .map(|interfaces| {
            interfaces
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default())
}

fn select_vm(discovery: &Discovery, selector: Option<&str>) -> Result<VmSummary> {
    if discovery.vms.is_empty() {
        bail!(
            "{}{}",
            format!("no QEMU D-Bus VMs found on the {}", discovery.bus_label),
            format_scan_warnings(&discovery.warnings)
        );
    }

    let Some(selector) = selector else {
        if discovery.vms.len() == 1 {
            return Ok(discovery.vms[0].clone());
        }

        bail!(
            "multiple QEMU D-Bus VMs are visible on the {}. Re-run with `--vm <NAME|UUID|OWNER>`.\nAvailable VMs:\n{}",
            discovery.bus_label,
            format_vm_choices(&discovery.vms)
        );
    };

    let matches = discovery
        .vms
        .iter()
        .filter(|vm| vm.name == selector || vm.uuid == selector || vm.owner.as_str() == selector)
        .cloned()
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => bail!(
            "no QEMU D-Bus VM matched `{selector}` on the {}.\nAvailable VMs:\n{}",
            discovery.bus_label,
            format_vm_choices(&discovery.vms)
        ),
        [vm] => Ok(vm.clone()),
        _ => bail!(
            "the selector `{selector}` matched multiple VMs on the {}.\nAvailable matches:\n{}",
            discovery.bus_label,
            format_vm_choices(&matches)
        ),
    }
}

fn select_console(report: &InspectionReport, console_id: Option<u32>) -> Result<ConsoleSummary> {
    if report.consoles.is_empty() {
        bail!(
            "the VM `{}` on {} does not report any display consoles",
            report.vm.name,
            report.bus_label
        );
    }

    match console_id {
        Some(console_id) => report
            .consoles
            .iter()
            .find(|console| console.id == console_id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "console {console_id} was not found on VM `{}`.\nAvailable consoles:\n{}",
                    report.vm.name,
                    format_console_choices(&report.consoles)
                )
            }),
        None => Ok(report.consoles[0].clone()),
    }
}

fn format_vm_choices(vms: &[VmSummary]) -> String {
    vms.iter()
        .map(|vm| {
            format!(
                "- {} | {} | {} | {}",
                vm.name, vm.uuid, vm.owner, vm.source_label
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_console_choices(consoles: &[ConsoleSummary]) -> String {
    consoles
        .iter()
        .map(|console| {
            format!(
                "- {} | {} | {}x{}",
                console.id, console.label, console.width, console.height
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_bus(address: Option<&str>) -> String {
    match address {
        Some(address) => format!("D-Bus at `{address}`"),
        None => "session bus".to_owned(),
    }
}

fn describe_scope(address: Option<&str>) -> String {
    match address {
        Some(address) => describe_bus(Some(address)),
        None => "session bus and detected libvirt D-Bus sockets".to_owned(),
    }
}

fn explain_address_connect_error<E>(address: &str, error: E) -> anyhow::Error
where
    E: StdError + Send + Sync + 'static,
{
    let mut message = format!("failed to connect to D-Bus address `{address}`");

    if caused_by_permission_denied(&error) {
        if let Some(path) = unix_socket_path_from_address(address) {
            message.push_str(&format!(
                "\nPermission denied opening `{path}`. Check search permission on the parent directories and connect permission on the socket."
            ));
        } else {
            message.push_str("\nPermission denied while opening the configured D-Bus address.");
        }
    }

    anyhow::Error::new(error).context(message)
}

fn caused_by_permission_denied(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);

    while let Some(err) = current {
        if let Some(io_error) = err.downcast_ref::<std::io::Error>() {
            if io_error.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
        }

        current = err.source();
    }

    false
}

fn unix_socket_path_from_address(address: &str) -> Option<&str> {
    address.strip_prefix("unix:path=")
}

fn discovery_targets() -> (Vec<DiscoveryTarget>, Vec<String>) {
    let mut targets = vec![DiscoveryTarget::session()];
    let (addresses, warnings) = libvirt_socket_addresses();
    targets.extend(addresses.into_iter().map(DiscoveryTarget::explicit_owned));
    (targets, warnings)
}

fn libvirt_socket_addresses() -> (Vec<String>, Vec<String>) {
    let mut paths = Vec::new();
    let mut warnings = Vec::new();

    for directory in LIBVIRT_DBUS_DIRS {
        match fs::read_dir(directory) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => paths.push(entry.path()),
                        Err(error) => warnings.push(format!("{directory}: {error}")),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => warnings.push(format!("{directory}: {error}")),
        }
    }

    (libvirt_socket_addresses_from_paths(paths), warnings)
}

fn libvirt_socket_addresses_from_paths<I>(paths: I) -> Vec<String>
where
    I: IntoIterator<Item = PathBuf>,
{
    let mut addresses = BTreeSet::new();

    for path in paths {
        if is_dbus_socket_path(&path) {
            addresses.insert(socket_path_to_address(&normalize_socket_path(&path)));
        }
    }

    addresses.into_iter().collect()
}

fn is_dbus_socket_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("sock")
}

fn socket_path_to_address(path: &Path) -> String {
    format!("unix:path={}", path.display())
}

fn normalize_socket_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn sort_vms(vms: &mut [VmSummary]) {
    vms.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.uuid.cmp(&right.uuid))
            .then(left.source_label.cmp(&right.source_label))
            .then(left.owner.as_str().cmp(right.owner.as_str()))
    });
}

fn format_scan_warnings(warnings: &[String]) -> String {
    if warnings.is_empty() {
        String::new()
    } else {
        format!("\nScan warnings:\n{}", warnings.join("\n"))
    }
}

pub(crate) fn clipboard_agent_status(chardevs: &[ChardevSummary]) -> ClipboardAgentStatus {
    let clipboard_channels = chardevs
        .iter()
        .filter(|chardev| is_clipboard_bridge_chardev(chardev))
        .collect::<Vec<_>>();

    if clipboard_channels.is_empty() {
        ClipboardAgentStatus::Unknown
    } else if clipboard_channels
        .iter()
        .any(|chardev| chardev.frontend_open)
    {
        ClipboardAgentStatus::Connected
    } else {
        ClipboardAgentStatus::GuestDisconnected
    }
}

pub(crate) fn suggested_inspection_warnings(report: &InspectionReport) -> Vec<String> {
    let mut warnings = Vec::new();

    if !report.has_audio {
        warnings.push(missing_audio_warning().to_owned());
    }

    if !report.has_clipboard {
        warnings.push(missing_clipboard_warning().to_owned());
    } else {
        match clipboard_agent_status(&report.chardevs) {
            ClipboardAgentStatus::Unknown => {}
            ClipboardAgentStatus::GuestDisconnected => {
                warnings.push(disconnected_clipboard_agent_warning().to_owned())
            }
            ClipboardAgentStatus::Connected => {}
        }
    }

    warnings
}

pub(crate) fn missing_audio_warning() -> &'static str {
    "QEMU does not expose the D-Bus audio object. QD2 audio needs `-audiodev driver=dbus,id=...`, `-display dbus,...,audiodev=...`, and a sound device wired with `audiodev=...`."
}

pub(crate) fn missing_clipboard_warning() -> &'static str {
    "QEMU does not expose the D-Bus clipboard object. Clipboard sharing usually needs a `qemu-vdagent` chardev with `clipboard=on` and a `virtserialport` named `com.redhat.spice.0`."
}

pub(crate) fn unverifiable_clipboard_agent_note() -> &'static str {
    "QEMU exposes the D-Bus clipboard object, but no clipboard/vdagent chardev is visible in the exported chardev list. Clipboard can still work in this setup, so QD2 cannot confirm the guest agent state from D-Bus alone."
}

pub(crate) fn disconnected_clipboard_agent_warning() -> &'static str {
    "A clipboard/vdagent channel is present, but the guest side is not connected. Make sure the guest vdagent service is running."
}

fn is_clipboard_bridge_chardev(chardev: &ChardevSummary) -> bool {
    let name = chardev.name.to_ascii_lowercase();
    let owner = chardev.owner.to_ascii_lowercase();

    name.contains("vdagent")
        || owner.contains("vdagent")
        || owner == "com.redhat.spice.0"
        || owner.contains("spice")
}

fn deduplicate_vms(vms: &mut Vec<VmSummary>) {
    let mut seen = BTreeSet::new();

    vms.retain(|vm| match dedup_key(vm) {
        Some(key) => seen.insert(key),
        None => true,
    });
}

fn dedup_key(vm: &VmSummary) -> Option<(String, String, Vec<u32>, Vec<String>)> {
    if vm.uuid.is_empty() {
        None
    } else {
        Some((
            vm.uuid.clone(),
            vm.name.clone(),
            vm.console_ids.clone(),
            vm.interfaces.clone(),
        ))
    }
}

impl DiscoveryTarget {
    fn session() -> Self {
        Self {
            label: describe_bus(None),
            address: None,
        }
    }

    fn explicit(address: &str) -> Self {
        Self::explicit_owned(address.to_owned())
    }

    fn explicit_owned(address: String) -> Self {
        Self {
            label: describe_bus(Some(&address)),
            address: Some(address),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vm(name: &str, uuid: &str, owner: &str) -> VmSummary {
        VmSummary {
            source_label: "session bus".to_owned(),
            source_address: None,
            owner: owner.try_into().unwrap(),
            name: name.to_owned(),
            uuid: uuid.to_owned(),
            console_ids: vec![0],
            interfaces: vec!["org.qemu.Display1.Console".to_owned()],
        }
    }

    #[test]
    fn single_vm_is_selected_by_default() {
        let vm = sample_vm("demo", "uuid-1", ":1.101");
        let discovery = Discovery {
            bus_label: describe_scope(None),
            vms: vec![vm.clone()],
            warnings: Vec::new(),
        };
        let selected = select_vm(&discovery, None).unwrap();

        assert_eq!(selected.name, "demo");
        assert_eq!(selected.uuid, "uuid-1");
    }

    #[test]
    fn selector_matches_name_uuid_and_owner() {
        let vm = sample_vm("demo", "uuid-1", ":1.101");
        let discovery = Discovery {
            bus_label: describe_scope(None),
            vms: vec![vm.clone()],
            warnings: Vec::new(),
        };

        assert_eq!(select_vm(&discovery, Some("demo")).unwrap().owner, vm.owner);
        assert_eq!(
            select_vm(&discovery, Some("uuid-1")).unwrap().owner,
            vm.owner
        );
        assert_eq!(
            select_vm(&discovery, Some(":1.101")).unwrap().owner,
            vm.owner
        );
    }

    #[test]
    fn duplicate_name_requires_more_specific_selector() {
        let discovery = Discovery {
            bus_label: describe_scope(None),
            vms: vec![
                sample_vm("demo", "uuid-1", ":1.101"),
                sample_vm("demo", "uuid-2", ":1.102"),
            ],
            warnings: Vec::new(),
        };

        let error = select_vm(&discovery, Some("demo")).unwrap_err().to_string();

        assert!(error.contains("matched multiple VMs"));
        assert!(error.contains("uuid-1"));
        assert!(error.contains("uuid-2"));
    }

    #[test]
    fn libvirt_candidates_only_keep_sock_entries() {
        let addresses = libvirt_socket_addresses_from_paths([
            PathBuf::from("/run/libvirt/qemu/dbus/12-oscp-dbus.sock"),
            PathBuf::from("/run/libvirt/qemu/dbus/ignore-me"),
            PathBuf::from("/run/libvirt/qemu/dbus/notes.txt"),
        ]);

        assert_eq!(
            addresses,
            vec!["unix:path=/run/libvirt/qemu/dbus/12-oscp-dbus.sock"]
        );
    }

    #[test]
    fn unix_socket_path_is_extracted_from_address() {
        assert_eq!(
            unix_socket_path_from_address("unix:path=/run/libvirt/qemu/dbus/test.sock"),
            Some("/run/libvirt/qemu/dbus/test.sock")
        );
        assert_eq!(unix_socket_path_from_address("tcp:host=localhost"), None);
    }

    #[test]
    fn duplicate_vm_entries_are_collapsed_by_uuid_fingerprint() {
        let mut vms = vec![
            VmSummary {
                source_label: "session bus".to_owned(),
                source_address: None,
                owner: ":1.101".try_into().unwrap(),
                name: "demo".to_owned(),
                uuid: "uuid-1".to_owned(),
                console_ids: vec![0],
                interfaces: vec!["org.qemu.Display1.Console".to_owned()],
            },
            VmSummary {
                source_label: "D-Bus at `unix:path=/run/libvirt/qemu/dbus/demo.sock`".to_owned(),
                source_address: Some("unix:path=/run/libvirt/qemu/dbus/demo.sock".to_owned()),
                owner: ":1.102".try_into().unwrap(),
                name: "demo".to_owned(),
                uuid: "uuid-1".to_owned(),
                console_ids: vec![0],
                interfaces: vec!["org.qemu.Display1.Console".to_owned()],
            },
        ];

        deduplicate_vms(&mut vms);

        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].source_label, "session bus");
    }

    fn sample_report(
        has_audio: bool,
        has_clipboard: bool,
        chardevs: Vec<ChardevSummary>,
    ) -> InspectionReport {
        InspectionReport {
            bus_label: describe_scope(None),
            vm: sample_vm("demo", "uuid-1", ":1.101"),
            has_audio,
            has_clipboard,
            consoles: vec![ConsoleSummary {
                id: 0,
                label: "Demo".to_owned(),
                head: 0,
                kind: "display".to_owned(),
                width: 1280,
                height: 720,
                interfaces: vec!["org.qemu.Display1.Mouse".to_owned()],
            }],
            chardevs,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn connected_vdagent_channel_is_detected() {
        let status = clipboard_agent_status(&[ChardevSummary {
            name: "vdagent".to_owned(),
            owner: "com.redhat.spice.0".to_owned(),
            frontend_open: true,
            echo: false,
        }]);

        assert_eq!(status, ClipboardAgentStatus::Connected);
    }

    #[test]
    fn disconnected_vdagent_channel_is_detected() {
        let status = clipboard_agent_status(&[ChardevSummary {
            name: "vdagent".to_owned(),
            owner: "com.redhat.spice.0".to_owned(),
            frontend_open: false,
            echo: false,
        }]);

        assert_eq!(status, ClipboardAgentStatus::GuestDisconnected);
    }

    #[test]
    fn missing_vdagent_export_is_treated_as_unknown_not_missing() {
        let status = clipboard_agent_status(&[]);

        assert_eq!(status, ClipboardAgentStatus::Unknown);
    }

    #[test]
    fn inspection_warnings_cover_missing_audio_and_disconnected_clipboard_agent() {
        let report = sample_report(
            false,
            true,
            vec![ChardevSummary {
                name: "vdagent".to_owned(),
                owner: "com.redhat.spice.0".to_owned(),
                frontend_open: false,
                echo: false,
            }],
        );

        let warnings = suggested_inspection_warnings(&report);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("audio object"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("guest side is not connected"))
        );
    }

    #[test]
    fn inspection_warnings_do_not_claim_missing_clipboard_agent_when_unverifiable() {
        let report = sample_report(true, true, Vec::new());

        let warnings = suggested_inspection_warnings(&report);
        assert!(warnings.is_empty());
    }

    #[test]
    fn inspection_warnings_cover_missing_clipboard_object() {
        let report = sample_report(true, false, Vec::new());

        let warnings = suggested_inspection_warnings(&report);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("clipboard object"));
    }
}
