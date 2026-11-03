use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use super::remote::RemoteConsole;
use crate::viewer::InputEvent;
use qemu_display::MouseButton;

struct Console {
    width: Arc<AtomicU32>,
}

#[zbus::interface(name = "org.qemu.Display1.Console")]
impl Console {
    #[zbus(property)]
    fn width(&self) -> u32 {
        self.width.load(Ordering::Relaxed)
    }
    #[zbus(property)]
    fn height(&self) -> u32 {
        600
    }
}

#[derive(Debug, PartialEq)]
enum MouseCall {
    Move(u32, u32),
    Press(u32),
}

struct Mouse {
    calls: Arc<Mutex<Vec<MouseCall>>>,
}

#[zbus::interface(name = "org.qemu.Display1.Mouse")]
impl Mouse {
    fn set_abs_position(&self, x: u32, y: u32) {
        self.calls.lock().unwrap().push(MouseCall::Move(x, y));
    }
    fn press(&self, button: u32) {
        self.calls.lock().unwrap().push(MouseCall::Press(button));
    }
}

#[tokio::test]
async fn click_refreshes_resized_sibling_before_mapping_and_pressing() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let left_width = Arc::new(AtomicU32::new(1000));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        let server = zbus::connection::Builder::unix_stream(server_socket)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .serve_at(
                "/org/qemu/Display1/Console_0",
                Console {
                    width: left_width.clone(),
                },
            )
            .unwrap()
            .serve_at(
                "/org/qemu/Display1/Console_1",
                Console {
                    width: Arc::new(AtomicU32::new(1000)),
                },
            )
            .unwrap()
            .serve_at(
                "/org/qemu/Display1/Console_1",
                Mouse {
                    calls: calls.clone(),
                },
            )
            .unwrap()
            .build();
        let client = zbus::connection::Builder::unix_stream(client_socket)
            .p2p()
            .build();
        let (_server, client) = tokio::try_join!(server, client).unwrap();
        let console = RemoteConsole::new(&client, "org.qemu", 1, &[0, 1], Vec::new())
            .await
            .unwrap();
        console.refresh_head_map().await.unwrap();
        left_width.store(3000, Ordering::Relaxed);

        // Hover uses the cached layout until its refresh interval expires.
        console
            .handle_input(InputEvent::MouseAbs {
                x: 100,
                y: 200,
                refresh_layout: false,
            })
            .await
            .unwrap();
        // A click must pick up the new dimensions immediately, before moving.
        console
            .handle_input(InputEvent::MouseAbs {
                x: 100,
                y: 200,
                refresh_layout: true,
            })
            .await
            .unwrap();
        console
            .handle_input(InputEvent::MousePress(MouseButton::Left))
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                MouseCall::Move(550, 200),
                MouseCall::Move(775, 200),
                MouseCall::Press(0),
            ]
        );
    })
    .await
    .expect("local D-Bus test timed out");
}
