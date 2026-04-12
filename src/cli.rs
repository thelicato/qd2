use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "qd2",
    version,
    about = "Inspect and connect to QEMU D-Bus display backends"
)]
pub struct Cli {
    /// Print extra diagnostics while discovering VMs or running the viewer.
    #[arg(long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List visible QEMU D-Bus VMs.
    List(BusArgs),
    /// Inspect one QEMU D-Bus VM and its exported objects.
    Inspect(InspectArgs),
    /// Check the host and VM for common QD2 setup problems.
    Doctor(DoctorArgs),
    /// Open a GTK4 window for one QEMU D-Bus console.
    Connect(ConnectArgs),
    /// Print the QD2 version.
    Version,
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

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub bus: BusArgs,

    /// VM selector: matches the QEMU VM name, UUID, or D-Bus owner.
    #[arg(long, short = 'v', value_name = "NAME|UUID|OWNER")]
    pub vm: Option<String>,
}

impl DoctorArgs {
    pub fn address(&self) -> Option<&str> {
        self.bus.address.as_deref()
    }
}

#[derive(Debug, Clone, Args)]
pub struct ConnectArgs {
    #[command(flatten)]
    pub bus: BusArgs,

    /// VM selector: matches the QEMU VM name, UUID, or D-Bus owner.
    #[arg(long, short = 'v', value_name = "NAME|UUID|OWNER")]
    pub vm: Option<String>,

    /// Console ID to open. Defaults to the first reported console.
    #[arg(long, short = 'c', value_name = "CONSOLE_ID")]
    pub console: Option<u32>,

    /// Override viewer hotkeys, for example:
    /// `toggle-fullscreen=ctrl+enter,release-cursor=ctrl+alt`
    #[arg(long, value_name = "ACTION=ACCEL[,ACTION=ACCEL...]")]
    pub hotkeys: Option<String>,
}

impl ConnectArgs {
    pub fn address(&self) -> Option<&str> {
        self.bus.address.as_deref()
    }
}
