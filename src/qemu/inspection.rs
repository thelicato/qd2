use anyhow::{Context, Result};
use qemu_display::{ConsoleProxy, Display};

use crate::diagnostics;

use super::{
    ChardevSummary, ConnectTarget, ConsoleSummary, InspectionReport, connect, discover,
    discovery::{interfaces_for_console, managed_objects},
    selection::{select_console, select_vm},
    suggested_inspection_warnings,
};

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
        console_id: console.id,
        width: console.width,
        height: console.height,
        console_interfaces: console.interfaces.clone(),
        warnings: report.warnings,
    })
}
