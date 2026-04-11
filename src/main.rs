mod cli;
mod qemu;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::List(args) => {
            let discovery = qemu::discover(args.address.as_deref()).await?;
            print_vm_list(&discovery);
        }
        Command::Inspect(args) => {
            let report = qemu::inspect(args.address(), args.vm.as_deref()).await?;
            print_inspection(&report);
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
