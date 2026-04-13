use super::{ChardevSummary, ClipboardAgentStatus, InspectionReport};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::{ConsoleSummary, VmSummary};

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

    fn sample_report(
        has_audio: bool,
        has_clipboard: bool,
        chardevs: Vec<ChardevSummary>,
    ) -> InspectionReport {
        InspectionReport {
            bus_label: "session bus and detected libvirt D-Bus sockets".to_owned(),
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
