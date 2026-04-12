mod cli;
mod diagnostics;
mod qemu;
mod viewer;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    diagnostics::set_verbose(cli.verbose);

    match cli.command {
        Command::List(args) => {
            let discovery = qemu::discover(args.address.as_deref()).await?;
            print_warnings(&discovery.warnings);
            print_vm_list(&discovery);
        }
        Command::Inspect(args) => {
            let report = qemu::inspect(args.address(), args.vm.as_deref()).await?;
            print_warnings(&report.warnings);
            print_inspection(&report);
        }
        Command::Doctor(args) => {
            let report = diagnostics::doctor(args.address(), args.vm.as_deref()).await;
            print_doctor(&report);
        }
        Command::Connect(args) => {
            let target = if let Some(selector) = args.vm.as_deref() {
                qemu::resolve_connect_target(args.address(), Some(selector), args.console).await?
            } else {
                let discovery = qemu::discover(args.address()).await?;

                match discovery.vms.as_slice() {
                    [] => qemu::resolve_connect_target(args.address(), None, args.console).await?,
                    [vm] => {
                        qemu::resolve_connect_target(args.address(), Some(&vm.uuid), args.console)
                            .await?
                    }
                    _ => {
                        let Some(vm) = viewer::choose_vm(&discovery.vms)? else {
                            return Ok(());
                        };
                        qemu::resolve_connect_target(args.address(), Some(&vm.uuid), args.console)
                            .await?
                    }
                }
            };
            print_warnings(&target.warnings);
            viewer::connect(target, args.address(), args.hotkeys.as_deref())?;
        }
        Command::Version => {
            println!("qd2 {}", env!("CARGO_PKG_VERSION"));
        }
    }

    Ok(())
}

fn print_vm_list(discovery: &qemu::Discovery) {
    if discovery.vms.is_empty() {
        println!(
            "No QEMU D-Bus VMs found on the {}.\nHint: start QEMU with `-display dbus` or pass `--address <DBUS_ADDRESS>`.",
            discovery.bus_label
        );
        return;
    }

    for (index, vm) in discovery.vms.iter().enumerate() {
        if index > 0 {
            println!();
        }

        println!("{}", vm.name);
        println!("  UUID: {}", vm.uuid);
        println!("  D-Bus owner: {}", vm.owner);
        println!("  Source: {}", vm.source_label);
        println!("  Consoles: {}", vm.console_ids.len());

        if vm.interfaces.is_empty() {
            println!("  Interfaces: none reported");
        } else {
            println!("  Interfaces: {}", vm.interfaces.join(", "));
        }
    }
}

fn print_inspection(report: &qemu::InspectionReport) {
    println!("{}", report.vm.name);
    println!("  UUID: {}", report.vm.uuid);
    println!("  D-Bus owner: {}", report.vm.owner);
    println!("  Bus: {}", report.bus_label);
    if let Some(address) = &report.vm.source_address {
        println!("  Address: {}", address);
    }

    if report.vm.interfaces.is_empty() {
        println!("  VM interfaces: none reported");
    } else {
        println!("  VM interfaces: {}", report.vm.interfaces.join(", "));
    }

    println!("  Audio object: {}", yes_no(report.has_audio));
    println!("  Clipboard object: {}", yes_no(report.has_clipboard));
    println!("  Chardevs: {}", report.chardevs.len());

    if report.consoles.is_empty() {
        println!();
        println!("No consoles reported.");
    } else {
        println!();
        println!("Consoles:");

        for console in &report.consoles {
            println!("  - Console {}: {}", console.id, console.label);
            println!(
                "    Type: {} | Head: {} | Size: {}x{}",
                console.kind, console.head, console.width, console.height
            );

            if console.interfaces.is_empty() {
                println!("    Interfaces: none reported");
            } else {
                println!("    Interfaces: {}", console.interfaces.join(", "));
            }
        }
    }

    if !report.chardevs.is_empty() {
        println!();
        println!("Chardevs:");

        for chardev in &report.chardevs {
            println!("  - {}", chardev.name);
            println!(
                "    Owner: {} | Frontend open: {} | Echo: {}",
                chardev.owner,
                yes_no(chardev.frontend_open),
                yes_no(chardev.echo)
            );
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("Warning: {warning}");
    }
}

fn print_doctor(report: &diagnostics::DoctorReport) {
    println!("Host checks:");
    for check in &report.host_checks {
        print_doctor_check(check);
    }

    if !report.vm_checks.is_empty() {
        println!();
        if let Some(inspection) = &report.inspected_vm {
            println!("VM checks for {}:", inspection.vm.name);
        } else {
            println!("VM checks:");
        }

        for check in &report.vm_checks {
            print_doctor_check(check);
        }
    }
}

fn print_doctor_check(check: &diagnostics::DoctorCheck) {
    let label = match check.status {
        diagnostics::DoctorStatus::Ok => "OK",
        diagnostics::DoctorStatus::Warn => "WARN",
        diagnostics::DoctorStatus::Fail => "FAIL",
    };

    let mut lines = check.detail.lines();
    if let Some(first_line) = lines.next() {
        println!("  [{label}] {}: {first_line}", check.name);
    } else {
        println!("  [{label}] {}", check.name);
    }

    for line in lines {
        println!("      {line}");
    }
}
