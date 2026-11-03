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

    /// Start the viewer directly in fullscreen mode.
    #[arg(long)]
    pub fullscreen: bool,

    /// Open the viewer without normal window decorations.
    #[arg(long)]
    pub undecorated: bool,

    /// Do not show the floating toolbar (or its top-edge hover hotspot) in
    /// fullscreen. Useful when the guest has panels at the screen edges.
    #[arg(long)]
    pub no_fullscreen_bar: bool,

    /// Use QEMU-provided DMABUF damage rectangles instead of full-surface
    /// refreshes. This can be faster, but some guest/driver combinations may
    /// flicker.
    #[arg(long = "dpu")]
    pub dmabuf_partial_updates: bool,

    /// Render through GTK without attempting compositor graphics offload.
    /// Useful as a fallback for compositor or driver rendering problems.
    #[arg(long)]
    pub no_offload: bool,

    /// Disable clipboard sharing in both directions.
    #[arg(long)]
    pub no_clipboard: bool,

    /// Set the GTK program identity used for application grouping and window rules.
    #[arg(long, value_name = "APP_ID", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    pub name: Option<String>,

    /// With `--fullscreen`: which monitor to fill, by connector name
    /// (`DP-1`, `HDMI-A-1`), model name, or 0-based index in the display's
    /// monitor list. Without it the window manager picks the monitor the
    /// window happens to appear on, which is not stable across launches.
    #[arg(long, value_name = "CONNECTOR|MODEL|INDEX")]
    pub monitor: Option<String>,

    /// Multi-head guests: where each console sits in the guest's desktop, in
    /// guest pixels, as `ID:X,Y;ID:X,Y` (e.g. `0:0,0;1:2560,0`). Default: heads
    /// side by side left-to-right in console order when scanout geometry is
    /// unavailable. Must match the guest's arrangement; negative offsets are
    /// supported for heads left of or above the origin.
    #[arg(long, value_name = "ID:X,Y;...")]
    pub head_layout: Option<String>,
}

impl ConnectArgs {
    pub fn address(&self) -> Option<&str> {
        self.bus.address.as_deref()
    }
}

/// Parse a `--head-layout` spec into `(console id, x, y)` triples.
pub fn parse_head_layout(spec: Option<&str>) -> anyhow::Result<Vec<(u32, i32, i32)>> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in spec.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let (id, pos) = entry
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("head layout entry `{entry}` must look like ID:X,Y"))?;
        let (x, y) = pos
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("head layout entry `{entry}` must look like ID:X,Y"))?;
        let id = id
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("bad console id in `{entry}`"))?;
        anyhow::ensure!(seen.insert(id), "duplicate console id {id} in head layout");
        out.push((
            id,
            x.trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("bad x in `{entry}`"))?,
            y.trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("bad y in `{entry}`"))?,
        ));
    }
    Ok(out)
}

pub fn validate_head_layout(layout: &[(u32, i32, i32)], console_ids: &[u32]) -> anyhow::Result<()> {
    for (id, _, _) in layout {
        anyhow::ensure!(
            console_ids.contains(id),
            "head layout refers to unknown graphical console {id}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod head_layout_tests {
    use super::{parse_head_layout, validate_head_layout};

    #[test]
    fn parses_pairs_and_ignores_blanks() {
        assert_eq!(
            parse_head_layout(Some(" 0:0,0; 1:2560,0 ;")).unwrap(),
            vec![(0, 0, 0), (1, 2560, 0)]
        );
        assert!(parse_head_layout(None).unwrap().is_empty());
        assert!(parse_head_layout(Some("  ")).unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_entries() {
        assert!(parse_head_layout(Some("1=2,3")).is_err());
        assert!(parse_head_layout(Some("1:2")).is_err());
        assert!(parse_head_layout(Some("x:2,3")).is_err());
    }

    #[test]
    fn negative_offsets_are_supported_and_duplicate_or_unknown_ids_are_rejected() {
        let layout = parse_head_layout(Some("0:0,0;1:-1920,-1080")).unwrap();
        assert_eq!(layout, vec![(0, 0, 0), (1, -1920, -1080)]);
        assert!(validate_head_layout(&layout, &[0, 1]).is_ok());
        assert!(
            validate_head_layout(&layout, &[0])
                .unwrap_err()
                .to_string()
                .contains("console 1")
        );
        assert!(
            parse_head_layout(Some("0:0,0;0:1920,0"))
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
    }
}
