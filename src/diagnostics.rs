use std::{
    env,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::qemu::{self, ClipboardAgentStatus, InspectionReport};

static VERBOSE: OnceLock<AtomicBool> = OnceLock::new();

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Clone, Debug)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub status: DoctorStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct DoctorReport {
    pub host_checks: Vec<DoctorCheck>,
    pub vm_checks: Vec<DoctorCheck>,
    pub inspected_vm: Option<InspectionReport>,
}

pub fn set_verbose(enabled: bool) {
    VERBOSE
        .get_or_init(|| AtomicBool::new(false))
        .store(enabled, Ordering::Relaxed);
}

pub fn verbose_enabled() -> bool {
    VERBOSE
        .get_or_init(|| AtomicBool::new(false))
        .load(Ordering::Relaxed)
}

pub fn verbose(message: impl AsRef<str>) {
    if verbose_enabled() {
        eprintln!("[qd2] {}", message.as_ref());
    }
}

pub async fn doctor(address: Option<&str>, selector: Option<&str>) -> DoctorReport {
    let mut report = DoctorReport::default();

    report.host_checks.push(graphics_session_check());
    report.host_checks.push(sudo_desktop_session_check());
    report.host_checks.push(audio_backend_check());

    let discovery = match qemu::discover(address).await {
        Ok(discovery) => {
            report.host_checks.push(discovery_check(&discovery));
            Some(discovery)
        }
        Err(error) => {
            report.host_checks.push(DoctorCheck {
                name: "QEMU D-Bus discovery",
                status: DoctorStatus::Fail,
                detail: format!("Could not inspect the configured D-Bus scope.\n{error:#}"),
            });
            None
        }
    };

    let Some(discovery) = discovery else {
        return report;
    };

    if discovery.vms.is_empty() {
        report.vm_checks.push(DoctorCheck {
            name: "VM-specific checks",
            status: DoctorStatus::Warn,
            detail: "No QEMU D-Bus VM is currently visible, so guest-side checks were skipped."
                .to_owned(),
        });
        return report;
    }

    if selector.is_none() && discovery.vms.len() > 1 {
        report.vm_checks.push(DoctorCheck {
            name: "VM selection",
            status: DoctorStatus::Warn,
            detail: format!(
                "Multiple VMs are visible on {}. Re-run `qd2 doctor --vm <NAME|UUID|OWNER>` for VM-specific checks.",
                discovery.bus_label
            ),
        });
        return report;
    }

    match qemu::inspect(address, selector).await {
        Ok(inspection) => {
            report.vm_checks.push(console_check(
                inspection.consoles.len(),
                &inspection.vm.name,
            ));
            report.vm_checks.push(audio_object_check(&inspection));
            report.vm_checks.push(clipboard_object_check(&inspection));
            if inspection.has_clipboard {
                report.vm_checks.push(clipboard_agent_check(&inspection));
            }
            report.inspected_vm = Some(inspection);
        }
        Err(error) => {
            report.vm_checks.push(DoctorCheck {
                name: "VM inspection",
                status: DoctorStatus::Fail,
                detail: format!("Could not inspect the selected VM.\n{error:#}"),
            });
        }
    }

    report
}

pub fn pipewire_sudo_environment_hint() -> Option<String> {
    pipewire_sudo_environment_hint_from(
        env::var("SUDO_USER").ok().as_deref(),
        env::var("SUDO_UID").ok().as_deref(),
        env::var("XDG_RUNTIME_DIR").ok().as_deref(),
        env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    )
}

pub(crate) fn pipewire_sudo_environment_hint_from(
    sudo_user: Option<&str>,
    sudo_uid: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    dbus_session_bus: Option<&str>,
) -> Option<String> {
    let sudo_user = sudo_user?;
    if preserved_user_session(sudo_uid, xdg_runtime_dir, dbus_session_bus) {
        return None;
    }

    Some(format!(
        "QD2 audio hint: `pw-play` is running under sudo without the `{sudo_user}` desktop audio session environment. \
PipeWire playback usually needs the user session vars. Try rerunning with:\n  \
sudo --preserve-env=XDG_RUNTIME_DIR,DBUS_SESSION_BUS_ADDRESS,WAYLAND_DISPLAY,PULSE_SERVER,PULSE_COOKIE qd2 connect ...\n  \
or run QD2 as your regular user after granting access to the QEMU D-Bus socket."
    ))
}

fn graphics_session_check() -> DoctorCheck {
    let wayland = env::var("WAYLAND_DISPLAY").ok();
    let x11 = env::var("DISPLAY").ok();
    let session_type = env::var("XDG_SESSION_TYPE").ok();
    let runtime_dir = env::var("XDG_RUNTIME_DIR").ok();

    match (wayland, x11) {
        (Some(wayland), _) => DoctorCheck {
            name: "Graphics session",
            status: DoctorStatus::Ok,
            detail: maybe_append_verbose_detail(
                format!("Wayland display `{wayland}` is set."),
                session_type,
                runtime_dir,
            ),
        },
        (None, Some(display)) => DoctorCheck {
            name: "Graphics session",
            status: DoctorStatus::Ok,
            detail: maybe_append_verbose_detail(
                format!("X11 display `{display}` is set."),
                session_type,
                runtime_dir,
            ),
        },
        (None, None) => DoctorCheck {
            name: "Graphics session",
            status: DoctorStatus::Fail,
            detail: maybe_append_verbose_detail(
                "Neither `WAYLAND_DISPLAY` nor `DISPLAY` is set, so `qd2 connect` is unlikely to open a GTK window.".to_owned(),
                session_type,
                runtime_dir,
            ),
        },
    }
}

fn sudo_desktop_session_check() -> DoctorCheck {
    let sudo_user = env::var("SUDO_USER").ok();
    let sudo_uid = env::var("SUDO_UID").ok();
    let runtime_dir = env::var("XDG_RUNTIME_DIR").ok();
    let dbus_session_bus = env::var("DBUS_SESSION_BUS_ADDRESS").ok();

    match sudo_user {
        None => DoctorCheck {
            name: "Desktop session ownership",
            status: DoctorStatus::Ok,
            detail: "QD2 is running as the current desktop user.".to_owned(),
        },
        Some(sudo_user) => {
            if preserved_user_session(
                sudo_uid.as_deref(),
                runtime_dir.as_deref(),
                dbus_session_bus.as_deref(),
            ) {
                DoctorCheck {
                    name: "Desktop session ownership",
                    status: DoctorStatus::Ok,
                    detail: format!(
                        "Running under sudo, but the `{sudo_user}` desktop session environment appears to be preserved."
                    ),
                }
            } else {
                DoctorCheck {
                    name: "Desktop session ownership",
                    status: DoctorStatus::Warn,
                    detail: format!(
                        "QD2 is running under sudo without the `{sudo_user}` desktop session environment. Wayland, PipeWire, and clipboard integration may fail.\nTry:\n  sudo --preserve-env=XDG_RUNTIME_DIR,DBUS_SESSION_BUS_ADDRESS,WAYLAND_DISPLAY,PULSE_SERVER,PULSE_COOKIE qd2 connect ..."
                    ),
                }
            }
        }
    }
}

fn audio_backend_check() -> DoctorCheck {
    let pw_play = command_in_path("pw-play");
    let aplay = command_in_path("aplay");

    match (pw_play, aplay) {
        (Some(pw_play), Some(aplay)) => DoctorCheck {
            name: "Host audio backends",
            status: DoctorStatus::Ok,
            detail: format!(
                "Found `pw-play` at `{}` and `aplay` at `{}`.",
                pw_play.display(),
                aplay.display()
            ),
        },
        (Some(pw_play), None) => DoctorCheck {
            name: "Host audio backends",
            status: DoctorStatus::Ok,
            detail: format!("Found `pw-play` at `{}`.", pw_play.display()),
        },
        (None, Some(aplay)) => DoctorCheck {
            name: "Host audio backends",
            status: DoctorStatus::Warn,
            detail: format!(
                "`pw-play` was not found on PATH, so QD2 will fall back to `aplay` at `{}`.",
                aplay.display()
            ),
        },
        (None, None) => DoctorCheck {
            name: "Host audio backends",
            status: DoctorStatus::Fail,
            detail:
                "Neither `pw-play` nor `aplay` was found on PATH, so guest audio playback cannot start."
                    .to_owned(),
        },
    }
}

fn discovery_check(discovery: &qemu::Discovery) -> DoctorCheck {
    let status = if discovery.vms.is_empty() || !discovery.warnings.is_empty() {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Ok
    };

    let mut detail = if discovery.vms.is_empty() {
        format!("No QEMU D-Bus VMs were found on {}.", discovery.bus_label)
    } else {
        format!(
            "Found {} VM(s) on {}.",
            discovery.vms.len(),
            discovery.bus_label
        )
    };

    if !discovery.warnings.is_empty() {
        detail.push_str("\nDiscovery warnings:");
        for warning in &discovery.warnings {
            detail.push_str(&format!("\n  - {warning}"));
        }
    }

    DoctorCheck {
        name: "QEMU D-Bus discovery",
        status,
        detail,
    }
}

fn console_check(console_count: usize, vm_name: &str) -> DoctorCheck {
    if console_count == 0 {
        DoctorCheck {
            name: "Display consoles",
            status: DoctorStatus::Fail,
            detail: format!("The VM `{vm_name}` does not report any display consoles."),
        }
    } else {
        DoctorCheck {
            name: "Display consoles",
            status: DoctorStatus::Ok,
            detail: format!("The VM `{vm_name}` reports {console_count} console(s)."),
        }
    }
}

fn audio_object_check(report: &InspectionReport) -> DoctorCheck {
    if report.has_audio {
        DoctorCheck {
            name: "QEMU audio object",
            status: DoctorStatus::Ok,
            detail: "QEMU exposes the D-Bus audio object.".to_owned(),
        }
    } else {
        DoctorCheck {
            name: "QEMU audio object",
            status: DoctorStatus::Warn,
            detail: qemu::missing_audio_warning().to_owned(),
        }
    }
}

fn clipboard_object_check(report: &InspectionReport) -> DoctorCheck {
    if report.has_clipboard {
        DoctorCheck {
            name: "QEMU clipboard object",
            status: DoctorStatus::Ok,
            detail: "QEMU exposes the D-Bus clipboard object.".to_owned(),
        }
    } else {
        DoctorCheck {
            name: "QEMU clipboard object",
            status: DoctorStatus::Warn,
            detail: qemu::missing_clipboard_warning().to_owned(),
        }
    }
}

fn clipboard_agent_check(report: &InspectionReport) -> DoctorCheck {
    match qemu::clipboard_agent_status(&report.chardevs) {
        ClipboardAgentStatus::Unknown => DoctorCheck {
            name: "Clipboard guest agent",
            status: DoctorStatus::Ok,
            detail: qemu::unverifiable_clipboard_agent_note().to_owned(),
        },
        ClipboardAgentStatus::Connected => DoctorCheck {
            name: "Clipboard guest agent",
            status: DoctorStatus::Ok,
            detail: "Detected a connected clipboard/vdagent channel.".to_owned(),
        },
        ClipboardAgentStatus::GuestDisconnected => DoctorCheck {
            name: "Clipboard guest agent",
            status: DoctorStatus::Warn,
            detail: qemu::disconnected_clipboard_agent_warning().to_owned(),
        },
    }
}

fn maybe_append_verbose_detail(
    mut detail: String,
    session_type: Option<String>,
    runtime_dir: Option<String>,
) -> String {
    if !verbose_enabled() {
        return detail;
    }

    detail.push_str(&format!(
        "\nXDG_SESSION_TYPE={}\nXDG_RUNTIME_DIR={}",
        display_option(session_type.as_deref()),
        display_option(runtime_dir.as_deref())
    ));
    detail
}

fn preserved_user_session(
    sudo_uid: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    dbus_session_bus: Option<&str>,
) -> bool {
    let runtime_matches_user_session = sudo_uid
        .zip(xdg_runtime_dir)
        .is_some_and(|(uid, runtime)| runtime == format!("/run/user/{uid}"));
    let dbus_matches_user_session = sudo_uid
        .zip(dbus_session_bus)
        .is_some_and(|(uid, address)| address.contains(&format!("/run/user/{uid}/bus")));

    runtime_matches_user_session && dbus_matches_user_session
}

fn command_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;

    env::split_paths(&path).find_map(|entry| {
        let candidate = entry.join(name);
        is_executable_file(&candidate).then_some(candidate)
    })
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        return path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }

    #[cfg(not(unix))]
    true
}

fn display_option(value: Option<&str>) -> &str {
    value.unwrap_or("<unset>")
}

#[cfg(test)]
mod tests {
    use super::pipewire_sudo_environment_hint_from;

    #[test]
    fn sudo_pipewire_hint_is_suppressed_with_preserved_user_session_env() {
        assert_eq!(
            pipewire_sudo_environment_hint_from(
                Some("alice"),
                Some("1000"),
                Some("/run/user/1000"),
                Some("unix:path=/run/user/1000/bus"),
            ),
            None
        );
    }

    #[test]
    fn sudo_pipewire_hint_is_emitted_when_root_session_env_is_used() {
        let hint = pipewire_sudo_environment_hint_from(
            Some("alice"),
            Some("1000"),
            Some("/run/user/0"),
            None,
        )
        .expect("expected a sudo + PipeWire hint");

        assert!(hint.contains("alice"));
        assert!(hint.contains("--preserve-env"));
        assert!(hint.contains("QEMU D-Bus socket"));
    }
}
