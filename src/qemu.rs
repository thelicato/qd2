use std::convert::TryFrom;

use anyhow::{Context, Result, bail};
use qemu_display::{ConsoleProxy, Display};
use zbus::{
    Connection, fdo,
    names::{OwnedUniqueName, WellKnownName},
    zvariant::OwnedObjectPath,
};

const DISPLAY_ROOT_PATH: &str = "/org/qemu/Display1";

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
    fn console_ids(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<String>>;
}

#[derive(Debug, Clone)]
pub struct Discovery {
    pub bus_label: String,
    pub vms: Vec<VmSummary>,
}

#[derive(Debug, Clone)]
pub struct VmSummary {
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

pub async fn discover(address: Option<&str>) -> Result<Discovery> {
    let connection = connect(address).await?;
    let vms = discover_vms(&connection).await?;

    Ok(Discovery {
        bus_label: describe_bus(address),
        vms,
    })
}

pub async fn inspect(address: Option<&str>, selector: Option<&str>) -> Result<InspectionReport> {
    let connection = connect(address).await?;
    let vms = discover_vms(&connection).await?;
    let vm = select_vm(&vms, selector, address)?;
    let display = Display::new(&connection, Some(vm.owner.clone()))
        .await
        .with_context(|| format!("failed to connect to QEMU display owner {}", vm.owner))?;
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

    Ok(InspectionReport {
        bus_label: describe_bus(address),
        vm,
        has_audio: display.audio().await?.is_some(),
        has_clipboard: display.clipboard().await?.is_some(),
        consoles,
        chardevs,
    })
}

async fn connect(address: Option<&str>) -> Result<Connection> {
    match address {
        Some(address) => zbus::connection::Builder::address(address)?
            .build()
            .await
            .with_context(|| format!("failed to connect to D-Bus address `{address}`")),
        None => Connection::session()
            .await
            .context("failed to connect to the session D-Bus"),
    }
}

async fn discover_vms(connection: &Connection) -> Result<Vec<VmSummary>> {
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

        let mut console_ids = proxy
            .console_ids()
            .await?
            .into_iter()
            .map(u32::from)
            .collect::<Vec<_>>();
        console_ids.sort_unstable();

        vms.push(VmSummary {
            owner,
            name: proxy.name().await?,
            uuid: proxy.uuid().await?,
            console_ids,
            interfaces,
        });
    }

    vms.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.uuid.cmp(&right.uuid))
            .then(left.owner.as_str().cmp(right.owner.as_str()))
    });

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

fn select_vm(
    vms: &[VmSummary],
    selector: Option<&str>,
    address: Option<&str>,
) -> Result<VmSummary> {
    if vms.is_empty() {
        bail!("no QEMU D-Bus VMs found on the {}", describe_bus(address));
    }

    let Some(selector) = selector else {
        if vms.len() == 1 {
            return Ok(vms[0].clone());
        }

        bail!(
            "multiple QEMU D-Bus VMs are visible on the {}. Re-run with `--vm <NAME|UUID|OWNER>`.\nAvailable VMs:\n{}",
            describe_bus(address),
            format_vm_choices(vms)
        );
    };

    let matches = vms
        .iter()
        .filter(|vm| vm.name == selector || vm.uuid == selector || vm.owner.as_str() == selector)
        .cloned()
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => bail!(
            "no QEMU D-Bus VM matched `{selector}` on the {}.\nAvailable VMs:\n{}",
            describe_bus(address),
            format_vm_choices(vms)
        ),
        [vm] => Ok(vm.clone()),
        _ => bail!(
            "the selector `{selector}` matched multiple VMs on the {}.\nAvailable matches:\n{}",
            describe_bus(address),
            format_vm_choices(&matches)
        ),
    }
}

fn format_vm_choices(vms: &[VmSummary]) -> String {
    vms.iter()
        .map(|vm| format!("- {} | {} | {}", vm.name, vm.uuid, vm.owner))
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_bus(address: Option<&str>) -> String {
    match address {
        Some(address) => format!("D-Bus at `{address}`"),
        None => "session bus".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vm(name: &str, uuid: &str, owner: &str) -> VmSummary {
        VmSummary {
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
        let selected = select_vm(std::slice::from_ref(&vm), None, None).unwrap();

        assert_eq!(selected.name, "demo");
        assert_eq!(selected.uuid, "uuid-1");
    }

    #[test]
    fn selector_matches_name_uuid_and_owner() {
        let vm = sample_vm("demo", "uuid-1", ":1.101");
        let vms = vec![vm.clone()];

        assert_eq!(select_vm(&vms, Some("demo"), None).unwrap().owner, vm.owner);
        assert_eq!(
            select_vm(&vms, Some("uuid-1"), None).unwrap().owner,
            vm.owner
        );
        assert_eq!(
            select_vm(&vms, Some(":1.101"), None).unwrap().owner,
            vm.owner
        );
    }

    #[test]
    fn duplicate_name_requires_more_specific_selector() {
        let vms = vec![
            sample_vm("demo", "uuid-1", ":1.101"),
            sample_vm("demo", "uuid-2", ":1.102"),
        ];

        let error = select_vm(&vms, Some("demo"), None).unwrap_err().to_string();

        assert!(error.contains("matched multiple VMs"));
        assert!(error.contains("uuid-1"));
        assert!(error.contains("uuid-2"));
    }
}
