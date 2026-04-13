use std::{
    collections::BTreeSet,
    convert::TryFrom,
    error::Error as StdError,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use zbus::{Connection, fdo, names::WellKnownName, zvariant::OwnedObjectPath};

use crate::diagnostics;

use super::{Discovery, VmSummary};

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

pub(super) async fn managed_objects(
    connection: &Connection,
    owner: &zbus::names::OwnedUniqueName,
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

pub(super) fn interfaces_for_console(
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
}
