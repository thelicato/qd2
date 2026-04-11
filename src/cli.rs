use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "qd2",
    version,
    about = "Inspect and connect to QEMU D-Bus display backends"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List visible QEMU D-Bus VMs.
    List(BusArgs),
    /// Inspect one QEMU D-Bus VM and its exported objects.
    Inspect(InspectArgs),
}

#[derive(Debug, Clone, Args)]
pub struct BusArgs {
    /// D-Bus address to connect to instead of the session bus.
    #[arg(long, value_name = "DBUS_ADDRESS")]
    pub address: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct InspectArgs {
    #[command(flatten)]
    pub bus: BusArgs,

    /// VM selector: matches the QEMU VM name, UUID, or D-Bus owner.
    #[arg(long, short = 'v', value_name = "NAME|UUID|OWNER")]
    pub vm: Option<String>,
}

impl InspectArgs {
    pub fn address(&self) -> Option<&str> {
        self.bus.address.as_deref()
    }
}
