use anyhow::{Result, bail};

use super::{ConsoleSummary, Discovery, InspectionReport, VmSummary};

pub(super) fn select_vm(discovery: &Discovery, selector: Option<&str>) -> Result<VmSummary> {
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

pub(super) fn select_console(
    report: &InspectionReport,
    console_id: Option<u32>,
) -> Result<ConsoleSummary> {
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

fn format_scan_warnings(warnings: &[String]) -> String {
    if warnings.is_empty() {
        String::new()
    } else {
        format!("\nScan warnings:\n{}", warnings.join("\n"))
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
            bus_label: "session bus and detected libvirt D-Bus sockets".to_owned(),
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
            bus_label: "session bus and detected libvirt D-Bus sockets".to_owned(),
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
            bus_label: "session bus and detected libvirt D-Bus sockets".to_owned(),
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
}
