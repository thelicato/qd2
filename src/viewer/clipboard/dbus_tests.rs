use std::{
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    time::Duration,
};

use zbus::{Connection, message::Header};

use super::{ClipboardContent, ClipboardSelection, register_clipboard_bridge};
use crate::viewer::events;

#[derive(Debug, PartialEq)]
struct Call {
    method: &'static str,
    destination: String,
}

struct Clipboard {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Clipboard {
    fn record(&self, method: &'static str, header: Header<'_>) {
        self.calls.lock().unwrap().push(Call {
            method,
            destination: header.destination().unwrap().to_string(),
        });
    }
}

#[zbus::interface(name = "org.qemu.Display1.Clipboard")]
impl Clipboard {
    fn register(&self, #[zbus(header)] header: Header<'_>) {
        self.record("Register", header);
    }

    fn grab(
        &self,
        _selection: u32,
        _serial: u32,
        _mimes: Vec<String>,
        #[zbus(header)] header: Header<'_>,
    ) {
        self.record("Grab", header);
    }
}

async fn peer(with_clipboard: bool) -> (Connection, Connection, Arc<Mutex<Vec<Call>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let mut server = zbus::connection::Builder::unix_stream(server_socket)
        .server(zbus::Guid::generate())
        .unwrap()
        .p2p()
        .serve_at("/org/qemu/Display1", zbus::fdo::ObjectManager)
        .unwrap();
    if with_clipboard {
        server = server
            .serve_at(
                "/org/qemu/Display1/Clipboard",
                Clipboard {
                    calls: calls.clone(),
                },
            )
            .unwrap();
    }
    let client = zbus::connection::Builder::unix_stream(client_socket)
        .p2p()
        .build();
    let (server, client) = tokio::try_join!(server.build(), client).unwrap();
    (server, client, calls)
}

#[tokio::test]
async fn registration_and_content_updates_use_each_selected_vm_destination() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for owner in [":1.42", ":1.99"] {
            let (_server, client, calls) = peer(true).await;
            let (tx, _rx) = events::channel().unwrap();
            let session = register_clipboard_bridge(&client, owner, tx, true)
                .await
                .unwrap()
                .unwrap();
            let mut content = ClipboardContent::default();
            content.merge_text("selected VM clipboard".into());
            session
                .update_host_content(ClipboardSelection::Clipboard, Some(content))
                .await
                .unwrap();
            assert_eq!(
                *calls.lock().unwrap(),
                vec![
                    Call {
                        method: "Register",
                        destination: owner.into()
                    },
                    Call {
                        method: "Grab",
                        destination: owner.into()
                    },
                ]
            );
        }
    })
    .await
    .expect("local clipboard D-Bus test timed out");
}

#[tokio::test]
async fn disabled_sharing_skips_owner_lookup_and_registration() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (_server, client, calls) = peer(true).await;
        let (tx, _rx) = events::channel().unwrap();
        // An invalid owner would fail if the disabled path tried to build a proxy.
        assert!(
            register_clipboard_bridge(&client, "not a bus name", tx, false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(calls.lock().unwrap().is_empty());
    })
    .await
    .expect("local clipboard D-Bus test timed out");
}

#[tokio::test]
async fn a_vm_without_a_clipboard_continues_without_registering() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (_server, client, calls) = peer(false).await;
        let (tx, _rx) = events::channel().unwrap();
        assert!(
            register_clipboard_bridge(&client, ":1.42", tx, true)
                .await
                .unwrap()
                .is_none()
        );
        assert!(calls.lock().unwrap().is_empty());
    })
    .await
    .expect("local clipboard D-Bus test timed out");
}
