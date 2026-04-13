use std::{sync::mpsc::Sender, time::Duration};

use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::qemu::ConnectTarget;

use super::super::{InputEvent, ViewerEvent};

const RECONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub(super) struct ReconnectPlan {
    pub(super) requested_address: Option<String>,
    vm_name: String,
    pub(super) vm_uuid: String,
    pub(super) console_id: u32,
}

impl ReconnectPlan {
    pub(super) fn new(target: &ConnectTarget, requested_address: Option<String>) -> Self {
        Self {
            requested_address,
            vm_name: target.vm_name.clone(),
            vm_uuid: target.vm_uuid.clone(),
            console_id: target.console_id,
        }
    }

    pub(super) fn waiting_message(&self) -> String {
        match &self.requested_address {
            Some(address) => format!(
                "Connection to `{}` was lost. Trying to reconnect on `{address}`.\nIf the VM restarted on a new private D-Bus socket, rerun QD2 without `--address` or pass the new address.",
                self.vm_name
            ),
            None => format!(
                "Connection to `{}` was lost. Waiting for the VM to come back...",
                self.vm_name
            ),
        }
    }

    pub(super) fn error_message(&self, error: &anyhow::Error) -> String {
        let retry_hint = match &self.requested_address {
            Some(_) => {
                "QD2 will keep retrying the same explicit address until the VM returns there."
            }
            None => "QD2 will keep auto-discovering the VM by UUID while it restarts.",
        };

        format!(
            "{}\nLast reconnect error: {error:#}\n{retry_hint}",
            self.waiting_message()
        )
    }
}

pub(super) async fn wait_for_reconnect_retry(
    input_rx: &mut tokio_mpsc::UnboundedReceiver<InputEvent>,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> bool {
    let sleep = tokio::time::sleep(RECONNECT_RETRY_INTERVAL);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            _ = &mut *shutdown_rx => return true,
            maybe_input = input_rx.recv() => {
                if maybe_input.is_none() {
                    return true;
                }
            }
            _ = &mut sleep => return false,
        }
    }
}

pub(super) fn send_status_if_changed(
    event_tx: &Sender<ViewerEvent>,
    last_status: &mut Option<String>,
    message: String,
) {
    if last_status.as_deref() == Some(message.as_str()) {
        return;
    }

    *last_status = Some(message.clone());
    let _ = event_tx.send(ViewerEvent::Status(message));
}

#[cfg(test)]
mod tests {
    use super::ReconnectPlan;
    use crate::qemu::ConnectTarget;

    fn connect_target() -> ConnectTarget {
        ConnectTarget {
            source_address: Some("unix:path=/run/libvirt/qemu/dbus/7-demo-dbus.sock".to_owned()),
            owner: ":1.42".to_owned(),
            vm_name: "demo".to_owned(),
            vm_uuid: "11111111-2222-3333-4444-555555555555".to_owned(),
            console_id: 0,
            width: 1280,
            height: 720,
            console_interfaces: vec!["org.qemu.Display1.Mouse".to_owned()],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn reconnect_wait_message_mentions_new_socket_hint_for_explicit_addresses() {
        let plan = ReconnectPlan::new(
            &connect_target(),
            Some("unix:path=/run/libvirt/qemu/dbus/7-demo-dbus.sock".to_owned()),
        );

        let message = plan.waiting_message();
        assert!(message.contains("demo"));
        assert!(message.contains("--address"));
        assert!(message.contains("new private D-Bus socket"));
    }

    #[test]
    fn reconnect_wait_message_mentions_vm_return_for_auto_discovery() {
        let plan = ReconnectPlan::new(&connect_target(), None);

        let message = plan.waiting_message();
        assert!(message.contains("demo"));
        assert!(message.contains("Waiting for the VM to come back"));
        assert!(!message.contains("--address"));
    }
}
