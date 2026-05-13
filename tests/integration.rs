use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use wlproxy::proto::{self, read_packet, write_packet, Packet};

// ---------------------------------------------------------------------------
// Proto functions over a real UnixStream pair
// ---------------------------------------------------------------------------

#[test]
fn packet_roundtrip_over_unix_stream() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

    let packet = Packet {
        id: 42,
        opcode: 3,
        body: vec![1, 2, 3, 4],
    };
    write_packet(&mut a, &packet).unwrap();
    drop(a);

    let result = read_packet(&mut b).unwrap().unwrap();
    assert_eq!(result, packet);
}

#[test]
fn multiple_packets_over_unix_stream() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    b.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

    let packets: Vec<Packet> = (0..5)
        .map(|i| Packet {
            id: i,
            opcode: i as u16,
            body: vec![i as u8; 4],
        })
        .collect();

    for p in &packets {
        write_packet(&mut a, p).unwrap();
    }
    drop(a);

    for expected in &packets {
        let received = read_packet(&mut b).unwrap().unwrap();
        assert_eq!(&received, expected);
    }
    assert!(read_packet(&mut b).unwrap().is_none());
}

#[test]
fn read_packet_returns_none_on_closed_stream() {
    let (a, mut b) = UnixStream::pair().unwrap();
    drop(a);
    assert!(read_packet(&mut b).unwrap().is_none());
}

#[test]
fn bidirectional_messages_over_unix_stream() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    b.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

    let msg_a = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAA],
    };
    let msg_b = Packet {
        id: 2,
        opcode: 1,
        body: vec![0xBB],
    };

    write_packet(&mut a, &msg_a).unwrap();
    write_packet(&mut b, &msg_b).unwrap();

    let recv_b = read_packet(&mut b).unwrap().unwrap();
    assert_eq!(recv_b, msg_a);

    let recv_a = read_packet(&mut a).unwrap().unwrap();
    assert_eq!(recv_a, msg_b);
}

#[test]
fn large_packet_over_unix_stream() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // Max body size: u16 message_size = body + 8 bytes header ≤ 65535.
    // Write from a thread so the OS socket buffer (which may be as small as
    // ~32KB on macOS) is drained concurrently by the main thread reading.
    let body = vec![0xABu8; 65527];
    let packet = Packet {
        id: u32::MAX,
        opcode: u16::MAX,
        body,
    };
    let handle = std::thread::spawn(move || {
        write_packet(&mut a, &packet).unwrap();
        drop(a);
    });

    let received = read_packet(&mut b).unwrap().unwrap();
    assert_eq!(received.id, u32::MAX);
    assert_eq!(received.opcode, u16::MAX);
    assert_eq!(received.body.len(), 65527);
    assert!(received.body.iter().all(|&b| b == 0xAB));
    handle.join().unwrap();
}

// ---------------------------------------------------------------------------
// Binary passthrough (end-to-end)
// ---------------------------------------------------------------------------

fn wlproxy_binary() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_wlproxy")
        .map(PathBuf::from)
        .expect("wlproxy binary not found — run `cargo test` (CARGO_BIN_EXE_wlproxy is always set by cargo test)")
}

fn connect_with_retry(path: &std::path::Path, timeout: Duration) -> UnixStream {
    let start = Instant::now();
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return stream,
            Err(_) if start.elapsed() >= timeout => {
                panic!("timed out connecting to {}", path.display());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[test]
fn wlproxy_basic_passthrough() {
    let dir = tempdir().unwrap();
    let upstream = dir.path().join("upstream.sock");
    let downstream = dir.path().join("downstream.sock");

    // Start mock compositor (listener for wlproxy's upstream connection).
    let mock_listener = std::os::unix::net::UnixListener::bind(&upstream).unwrap();

    // Launch wlproxy.
    let mut wlproxy = Command::new(wlproxy_binary())
        .args([
            "--upstream",
            upstream.to_str().unwrap(),
            downstream.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start wlproxy");

    // Connect as a client to wlproxy's downstream socket.
    let mut client = connect_with_retry(&downstream, Duration::from_secs(5));

    // Mock compositor accepts wlproxy's connection.
    let (mut compositor, _) = mock_listener.accept().unwrap();

    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    compositor
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Send message client → wlproxy → compositor.
    let sent = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAB, 0xCD, 0x00, 0x00],
    };
    write_packet(&mut client, &sent).unwrap();
    let received = read_packet(&mut compositor).unwrap().unwrap();
    assert_eq!(received, sent, "client→compositor passthrough failed");

    // Send message compositor → wlproxy → client.
    // Use opcode=0 (no special handling on Display) with a non-empty body.
    let reply = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAA],
    };
    write_packet(&mut compositor, &reply).unwrap();
    let received = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(received, reply, "compositor→client passthrough failed");

    // Cleanup.
    wlproxy.kill().unwrap();
    let output = wlproxy.wait_with_output().unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("wlproxy stderr:\n{stderr}");
        }
    }
}

#[test]
fn wlproxy_multiple_concurrent_connections() {
    let dir = tempdir().unwrap();
    let upstream = dir.path().join("upstream.sock");
    let downstream = dir.path().join("downstream.sock");
    let mock_listener = std::os::unix::net::UnixListener::bind(&upstream).unwrap();

    // Start wlproxy.
    let mut wlproxy = Command::new(wlproxy_binary())
        .args([
            "--upstream",
            upstream.to_str().unwrap(),
            downstream.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start wlproxy");

    // Connect two clients and accept their upstream connections.
    let mut client1 = connect_with_retry(&downstream, Duration::from_secs(5));
    let (mut compositor1, _) = mock_listener.accept().unwrap();

    let mut client2 = connect_with_retry(&downstream, Duration::from_secs(5));
    let (mut compositor2, _) = mock_listener.accept().unwrap();

    for c in [&client1, &client2, &compositor1, &compositor2] {
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    // Send message client1 → compositor1.
    let msg1 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0x11, 0x22, 0x00, 0x00],
    };
    write_packet(&mut client1, &msg1).unwrap();
    assert_eq!(
        read_packet(&mut compositor1).unwrap().unwrap(),
        msg1,
        "client1 → compositor1"
    );

    // Send message client2 → compositor2.
    let msg2 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0x33, 0x44, 0x00, 0x00],
    };
    write_packet(&mut client2, &msg2).unwrap();
    assert_eq!(
        read_packet(&mut compositor2).unwrap().unwrap(),
        msg2,
        "client2 → compositor2"
    );

    // Send message compositor1 → client1.
    let reply1 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAA],
    };
    write_packet(&mut compositor1, &reply1).unwrap();
    assert_eq!(
        read_packet(&mut client1).unwrap().unwrap(),
        reply1,
        "compositor1 → client1"
    );

    // Send message compositor2 → client2.
    let reply2 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xBB],
    };
    write_packet(&mut compositor2, &reply2).unwrap();
    assert_eq!(
        read_packet(&mut client2).unwrap().unwrap(),
        reply2,
        "compositor2 → client2"
    );

    // Cleanup.
    wlproxy.kill().unwrap();
    let _ = wlproxy.wait_with_output();
}

// ---------------------------------------------------------------------------
// Helpers for wlproxy end-to-end tests
// ---------------------------------------------------------------------------

/// Spawn wlproxy with the given extra args (beyond --upstream)
/// and accept its upstream connection against the mock listener.
/// Returns (wlproxy_child, compositor_stream, client_stream).
fn spawn_wlproxy(
    extra_args: &[&str],
    dir: &std::path::Path,
    mock_listener: &UnixListener,
) -> (std::process::Child, UnixStream, UnixStream) {
    let upstream = dir.join("upstream.sock");
    let downstream = dir.join("downstream.sock");

    let wlproxy = Command::new(wlproxy_binary())
        .args([
            "--upstream",
            upstream.to_str().unwrap(),
            downstream.to_str().unwrap(),
        ])
        .args(extra_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start wlproxy");

    let client = connect_with_retry(&downstream, Duration::from_secs(5));

    let (compositor, _) = mock_listener.accept().unwrap();

    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    compositor
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    (wlproxy, compositor, client)
}

/// Send the standard Wayland object-chain messages that make wlproxy
/// recognise a new XdgToplevel, **and forward each request to compositor**
/// (reading it from client before the compositor would normally see it).
///
/// Returns the client itself so callers can continue sending messages.
fn build_object_chain(client: &mut UnixStream, compositor: &mut UnixStream) {
    // 1. Display.get_registry (opcode=1) → creates registry at id 2.
    write_packet(
        client,
        &Packet {
            id: 1,
            opcode: 1,
            body: {
                let mut b = vec![];
                proto::write_arg_uint(&mut b, 2).unwrap();
                b
            },
        },
    )
    .unwrap();
    let _ = read_packet(compositor).unwrap().unwrap();

    // 2. Compositor sends Registry.global (opcode=0) for xdg_wm_base, type_id=0.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        write_packet(
            compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(client).unwrap().unwrap();

    // 3. Client sends Registry.bind (opcode=0, id 2) → binds xdg_wm_base at id 3.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        write_packet(
            client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(compositor).unwrap().unwrap();

    // 4. XdgWmBase.get_xdg_surface (opcode=2, id 3) → surface at id 4.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(
            client,
            &Packet {
                id: 3,
                opcode: 2,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(compositor).unwrap().unwrap();

    // 5. XdgSurface.create_toplevel (opcode=1, id 4) → toplevel at id 5.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        write_packet(
            client,
            &Packet {
                id: 4,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(compositor).unwrap().unwrap();
}

/// Kill the wlproxy child and print stderr if the exit code is non-zero.
fn cleanup_wlproxy(mut wlproxy: std::process::Child) {
    wlproxy.kill().unwrap();
    let output = wlproxy.wait_with_output().unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("wlproxy stderr:\n{stderr}");
        }
    }
}

#[test]
fn wlproxy_object_chain_and_app_id_replacement() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) =
        spawn_wlproxy(&["--app-id", "filtered"], dir.path(), &mock_listener);

    build_object_chain(&mut client, &mut compositor);

    // Client sends XdgToplevel.set_app_id (opcode=3, id 5) with original app_id "my-app".
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 3,
                body,
            },
        )
        .unwrap();
    }
    let modified = read_packet(&mut compositor).unwrap().unwrap();

    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("filtered"),
        "app_id replacement failed: got {replaced:?}"
    );

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_title_replacement() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) =
        spawn_wlproxy(&["--title", "filtered-title"], dir.path(), &mock_listener);

    build_object_chain(&mut client, &mut compositor);

    // Client sends XdgToplevel.set_title (opcode=2, id 5) with original title "my-title".
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-title").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 2,
                body,
            },
        )
        .unwrap();
    }
    let modified = read_packet(&mut compositor).unwrap().unwrap();

    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("filtered-title"),
        "title replacement failed: got {replaced:?}"
    );

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_app_id_prefix() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &["--app-id", "pfx-", "--prefix-app-id"],
        dir.path(),
        &mock_listener,
    );

    build_object_chain(&mut client, &mut compositor);

    // Client sends XdgToplevel.set_app_id (opcode=3) with original app_id "my-app".
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 3,
                body,
            },
        )
        .unwrap();
    }
    let modified = read_packet(&mut compositor).unwrap().unwrap();

    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("pfx-my-app"),
        "app_id prefix failed: got {replaced:?}"
    );

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_title_prefix() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &["--title", "pfx-", "--prefix-title"],
        dir.path(),
        &mock_listener,
    );

    build_object_chain(&mut client, &mut compositor);

    // Client sends XdgToplevel.set_title (opcode=2) with original title "my-title".
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-title").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 2,
                body,
            },
        )
        .unwrap();
    }
    let modified = read_packet(&mut compositor).unwrap().unwrap();

    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("pfx-my-title"),
        "title prefix failed: got {replaced:?}"
    );

    cleanup_wlproxy(wlproxy);
}

// ---------------------------------------------------------------------------
// Object removal tracking (Display.delete_id)
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_delete_id() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) =
        spawn_wlproxy(&["--app-id", "filtered"], dir.path(), &mock_listener);

    build_object_chain(&mut client, &mut compositor);

    // Compositor sends Display.delete_id for obj 5 (XdgToplevel).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    // Client drains the forwarded delete_id (server→client thread removes obj 5 first).
    let _ = read_packet(&mut client).unwrap().unwrap();

    // Client sends set_app_id for obj 5 → should passthrough UNMODIFIED
    // because obj 5 is no longer tracked.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 3,
                body,
            },
        )
        .unwrap();
    }
    let received = read_packet(&mut compositor).unwrap().unwrap();
    let mut cursor = std::io::Cursor::new(&received.body[..]);
    let app_id = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        app_id.as_deref(),
        Some("my-app"),
        "set_app_id should pass through unmodified after delete_id: got {app_id:?}"
    );

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_delete_id_registry() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) =
        spawn_wlproxy(&["--app-id", "filtered"], dir.path(), &mock_listener);

    // 1. Client sends Display.get_registry (opcode=1, id 1) → Registry at id 2.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 2. Compositor sends globals on registry (id 2), including xdg_wm_base.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut client).unwrap().unwrap();

    // 3. Compositor deletes registry (id 2).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut client).unwrap().unwrap();

    // 4. Client sends get_registry again → new Registry at id 3.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 3).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 5. Compositor sends global on new registry (id 3) for wl_compositor.
    //    This validates that cache_reg_id was cleared — otherwise
    //    the global would be ignored because it's on id 3, not cached id 2.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 3,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let p = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(p.id, 3, "global should arrive on new registry id 3");
    assert_eq!(p.opcode, 0);
    let mut cursor = std::io::Cursor::new(&p.body);
    let _tid = proto::read_arg_uint(&mut cursor).unwrap();
    let name = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        name.as_deref(),
        Some("wl_compositor"),
        "global should be forwarded on new registry"
    );

    cleanup_wlproxy(wlproxy);
}

// ---------------------------------------------------------------------------
// Global event filtering — only xdg_wm_base objects are tracked
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_global_filtering() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) =
        spawn_wlproxy(&["--app-id", "filtered"], dir.path(), &mock_listener);

    // ---- Custom object chain with multiple globals ----

    // 0. Client sends Display.get_registry (opcode=1, id 1) → Registry at id 2.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 1. Compositor sends global for wl_compositor (NOT xdg_wm_base, type_id=0).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut client).unwrap().unwrap();

    // 2. Compositor sends global for xdg_wm_base (type_id=1).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut client).unwrap().unwrap();

    // 3. Client binds wl_compositor (type_id=0) → obj 3 (NOT tracked, wrong type_id).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 4. Client binds xdg_wm_base (type_id=1) → obj 4 (SHOULD be tracked).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 5. XdgWmBase.get_xdg_surface (opcode=2, id 4) → surface at id 5.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 4,
                opcode: 2,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 6. XdgSurface.create_toplevel (opcode=1, id 5) → toplevel at id 6.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 6).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 7. set_app_id on obj 3 (NOT tracked) → passthrough UNMODIFIED.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app-3").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 3,
                opcode: 3,
                body,
            },
        )
        .unwrap();
    }
    {
        let received = read_packet(&mut compositor).unwrap().unwrap();
        let mut cursor = std::io::Cursor::new(&received.body[..]);
        let app_id = proto::read_arg_string(&mut cursor).unwrap();
        assert_eq!(
            app_id.as_deref(),
            Some("my-app-3"),
            "obj 3 (wl_compositor) should pass through unmodified: got {app_id:?}"
        );
    }

    // 8. set_app_id on obj 6 (XdgToplevel) → REPLACED.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "some-app").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 6,
                opcode: 3,
                body,
            },
        )
        .unwrap();
    }
    {
        let received = read_packet(&mut compositor).unwrap().unwrap();
        let mut cursor = std::io::Cursor::new(&received.body[..]);
        let app_id = proto::read_arg_string(&mut cursor).unwrap();
        assert_eq!(
            app_id.as_deref(),
            Some("filtered"),
            "obj 6 (XdgToplevel) should have app_id replaced: got {app_id:?}"
        );
    }

    cleanup_wlproxy(wlproxy);
}

// ---------------------------------------------------------------------------
// FD forwarding in server→client direction
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_fd_forwarding_server_to_client() {
    use std::io::Read;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use uds::UnixStreamExt;

    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, compositor, mut client) = spawn_wlproxy(&[], dir.path(), &mock_listener);

    // Create a dummy socket pair — one end's FD is sent through wlproxy.
    let (dummy_send, dummy_recv) = std::os::unix::net::UnixStream::pair().unwrap();
    let send_fd = dummy_send.as_raw_fd();

    // Build a minimal valid Wayland packet (empty body → 8 bytes total).
    let mut packet_bytes = vec![];
    write_packet(
        &mut packet_bytes,
        &Packet {
            id: 1,
            opcode: 0,
            body: vec![],
        },
    )
    .unwrap();
    assert_eq!(packet_bytes.len(), 8);

    // Compositor sends the packet with an FD attached.
    compositor.send_fds(&packet_bytes, &[send_fd]).unwrap();
    drop(dummy_send);

    // Client reads from downstream (via wlproxy).
    // wlproxy should forward both data and the FD.
    // AncillaryWriter sends header_word1 (4 bytes + FD) and header_word2 (4 bytes)
    // as separate write() calls — recv_fds may return only the first chunk.
    let mut buf = [0u8; 8];
    let mut fd_buf = [0i32; 8];
    let (n, nfds) = client.recv_fds(&mut buf, &mut fd_buf).unwrap();
    assert!(nfds == 1, "FD should be forwarded by wlproxy");
    assert!(fd_buf[0] > 0, "received FD should be valid");
    if n < 8 {
        client.read_exact(&mut buf[n..]).unwrap();
    }

    // Parse and verify the packet content.
    let mut cursor = std::io::Cursor::new(&buf[..]);
    let received = read_packet(&mut cursor).unwrap().unwrap();
    assert_eq!(
        received,
        Packet {
            id: 1,
            opcode: 0,
            body: vec![]
        }
    );

    // Close the received FD.
    drop(unsafe { OwnedFd::from_raw_fd(fd_buf[0]) });
    drop(dummy_recv);

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_fd_forwarding_client_to_server() {
    use std::io::Read;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use uds::UnixStreamExt;

    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, client) = spawn_wlproxy(&[], dir.path(), &mock_listener);

    // Create a dummy socket pair — one end's FD is sent through wlproxy.
    let (dummy_send, dummy_recv) = std::os::unix::net::UnixStream::pair().unwrap();
    let send_fd = dummy_send.as_raw_fd();

    // Build a minimal valid Wayland packet (empty body → 8 bytes total).
    let mut packet_bytes = vec![];
    write_packet(
        &mut packet_bytes,
        &Packet {
            id: 1,
            opcode: 0,
            body: vec![],
        },
    )
    .unwrap();
    assert_eq!(packet_bytes.len(), 8);

    // Client sends the packet with an FD attached, through wlproxy.
    client.send_fds(&packet_bytes, &[send_fd]).unwrap();
    drop(dummy_send);

    // Compositor reads from upstream (via wlproxy).
    // wlproxy should forward both data and the FD.
    let mut buf = [0u8; 8];
    let mut fd_buf = [0i32; 8];
    let (n, nfds) = compositor.recv_fds(&mut buf, &mut fd_buf).unwrap();
    assert!(nfds == 1, "FD should be forwarded by wlproxy");
    assert!(fd_buf[0] > 0, "received FD should be valid");
    if n < 8 {
        compositor.read_exact(&mut buf[n..]).unwrap();
    }

    // Parse and verify the packet content.
    let mut cursor = std::io::Cursor::new(&buf[..]);
    let received = read_packet(&mut cursor).unwrap().unwrap();
    assert_eq!(
        received,
        Packet {
            id: 1,
            opcode: 0,
            body: vec![]
        }
    );

    // Close the received FD.
    drop(unsafe { OwnedFd::from_raw_fd(fd_buf[0]) });
    drop(dummy_recv);

    cleanup_wlproxy(wlproxy);
}

// ---------------------------------------------------------------------------
// Interface blocking
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_block_interfaces() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &[
            "--block",
            "zwlr_layer_shell_v1,ext_data_control_manager_v1,zwlr_screencopy_manager_v1",
        ],
        dir.path(),
        &mock_listener,
    );

    // Helper to send a global event from the compositor.
    let send_global = |compositor: &mut UnixStream, type_id: u32, name: &str, version: u32| {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, type_id).unwrap();
        proto::write_arg_string(&mut body, name).unwrap();
        proto::write_arg_uint(&mut body, version).unwrap();
        write_packet(
            compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    };

    // 1. Create registry via get_registry (opcode=1, id 1 → registry at id 2).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    // Drain from compositor side.
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 2. Compositor sends globals: some for blocked interfaces, some not.
    send_global(&mut compositor, 0, "wl_compositor", 4);
    send_global(&mut compositor, 1, "zwlr_layer_shell_v1", 5); // BLOCKED
    send_global(&mut compositor, 2, "xdg_wm_base", 6);
    send_global(&mut compositor, 3, "ext_data_control_manager_v1", 1); // BLOCKED
    send_global(&mut compositor, 4, "wl_shm", 1);
    send_global(&mut compositor, 5, "zwlr_screencopy_manager_v1", 3); // BLOCKED

    // 3. Sentinel: a non-global packet to mark the end.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }

    // 4. Client reads globals — should see only wl_compositor, xdg_wm_base, wl_shm.
    //    Each read confirms the blocked interfaces were silently dropped in between.
    for &expected in &["wl_compositor", "xdg_wm_base", "wl_shm"] {
        let p = read_packet(&mut client).unwrap().unwrap();
        assert_eq!(p.id, 2, "expected global on registry (id 2)");
        assert_eq!(p.opcode, 0, "expected global event (opcode 0)");
        let mut cursor = std::io::Cursor::new(&p.body);
        let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
        let name = proto::read_arg_string(&mut cursor).unwrap();
        assert_eq!(
            name.as_deref(),
            Some(expected),
            "unexpected global: got {name:?}, expected {expected}"
        );
    }

    // 5. Next packet should be the sentinel, NOT a global for a blocked interface.
    let sentinel = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(
        sentinel,
        Packet {
            id: 1,
            opcode: 1,
            body: {
                let mut b = vec![];
                proto::write_arg_uint(&mut b, 999).unwrap();
                b
            },
        },
        "expected sentinel after allowed interfaces"
    );

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_block_interfaces_app_id_still_works() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    // Block unrelated interfaces; app_id replacement should still work.
    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &[
            "--app-id",
            "filtered",
            "--block",
            "zwlr_layer_shell_v1,ext_data_control_manager_v1",
        ],
        dir.path(),
        &mock_listener,
    );

    build_object_chain(&mut client, &mut compositor);

    // Send set_app_id and verify it's replaced.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 3,
                body,
            },
        )
        .unwrap();
    }
    let modified = read_packet(&mut compositor).unwrap().unwrap();
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("filtered"),
        "app_id replacement should work alongside interface blocking: got {replaced:?}"
    );

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_block_interfaces_does_not_leak_fds() {
    use std::os::unix::io::AsRawFd;
    use uds::UnixStreamExt;

    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &["--block", "zwlr_layer_shell_v1"],
        dir.path(),
        &mock_listener,
    );

    // 1. Create registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 2. Send a global for a blocked interface with an FD attached.
    let mut global_body = vec![];
    proto::write_arg_uint(&mut global_body, 0).unwrap();
    proto::write_arg_string(&mut global_body, "zwlr_layer_shell_v1").unwrap();
    proto::write_arg_uint(&mut global_body, 5).unwrap();
    let mut packet_bytes = vec![];
    write_packet(
        &mut packet_bytes,
        &Packet {
            id: 2,
            opcode: 0,
            body: global_body,
        },
    )
    .unwrap();

    let (dummy_send, dummy_recv) = std::os::unix::net::UnixStream::pair().unwrap();
    let send_fd = dummy_send.as_raw_fd();
    compositor.send_fds(&packet_bytes, &[send_fd]).unwrap();
    drop(dummy_send);

    // 3. Send a global for a non-blocked interface (to verify fd consumption didn't break forwarding).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }

    // 4. Send a sentinel to mark the end.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }

    // 5. Client reads — should see wl_compositor (blocked interface's FD was dropped).
    let p = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(p.id, 2);
    assert_eq!(p.opcode, 0);
    let mut cursor = std::io::Cursor::new(&p.body);
    let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
    let name = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(name.as_deref(), Some("wl_compositor"));

    // 6. Verify sentinel (confirms no extra packets leaked past the blocked one).
    let sentinel = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(sentinel.id, 1);
    assert_eq!(sentinel.opcode, 1);
    let mut cursor = std::io::Cursor::new(&sentinel.body);
    assert_eq!(proto::read_arg_uint(&mut cursor).unwrap(), 999);

    drop(dummy_recv);
    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_block_interfaces_and_title_prefix() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    // Block unrelated interfaces; title prefix should still work.
    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &[
            "--title",
            "pfx-",
            "--prefix-title",
            "--block",
            "zwlr_layer_shell_v1",
        ],
        dir.path(),
        &mock_listener,
    );

    build_object_chain(&mut client, &mut compositor);

    // Send set_title and verify it's prefixed.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-title").unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 2,
                body,
            },
        )
        .unwrap();
    }
    let modified = read_packet(&mut compositor).unwrap().unwrap();
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("pfx-my-title"),
        "title prefix should work alongside interface blocking: got {replaced:?}"
    );

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_unknown_interface_warning() {
    let dir = tempdir().unwrap();
    let upstream = dir.path().join("upstream.sock");
    let downstream = dir.path().join("downstream.sock");

    // Start wlproxy with an unknown interface (not in known_protocols.txt).
    let mut wlproxy = Command::new(wlproxy_binary())
        .args([
            "--upstream",
            upstream.to_str().unwrap(),
            downstream.to_str().unwrap(),
            "--block",
            "UnknownInterface",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start wlproxy");

    // Give wlproxy time to print warnings (eprintln! flushes on newline
    // even when piped in Rust's stdio implementation), then kill it.
    std::thread::sleep(Duration::from_millis(500));
    wlproxy.kill().unwrap();
    let output = wlproxy.wait_with_output().unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown Wayland interface"),
        "expected warning about unknown interface in stderr, got: {stderr}"
    );
}

#[test]
fn wlproxy_block_interfaces_blocks_bind_requests() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &["--block", "zwlr_layer_shell_v1"],
        dir.path(),
        &mock_listener,
    );

    // 1. Create registry via get_registry (opcode=1, id 1 → registry at id 2).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    // Drain on compositor side.
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 2. Compositor announces globals: xdg_wm_base (allowed) and zwlr_layer_shell_v1 (blocked).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "zwlr_layer_shell_v1").unwrap();
        proto::write_arg_uint(&mut body, 5).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }
    // Drain client-side globals (second is blocked, so only 1 arrives).
    let global = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(global.id, 2);
    assert_eq!(global.opcode, 0);
    let mut cursor = std::io::Cursor::new(&global.body);
    let _tid = proto::read_arg_uint(&mut cursor).unwrap();
    let name = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        name.as_deref(),
        Some("xdg_wm_base"),
        "only xdg_wm_base interface should reach client"
    );

    // 3. Client tries to bind zwlr_layer_shell_v1 (bypass attempt).
    //    The bind includes the interface name — wlproxy should intercept it.
    let blocked_obj_id = 3u32;
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "zwlr_layer_shell_v1").unwrap();
        proto::write_arg_uint(&mut body, 5).unwrap();
        proto::write_arg_uint(&mut body, blocked_obj_id).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }

    // 4. Client also sends a legitimate bind for xdg_wm_base (should pass through).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }

    // 5. Send sentinel from compositor to verify client is still alive.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }

    // 6. Compositor should receive: get_registry, xdg_wm_base bind (NOT zwlr_layer_shell_v1 bind).
    // Wait — we already drained get_registry in step 1. So compositor's next read should be
    // the xdg_wm_base bind (the zwlr_layer_shell_v1 bind was dropped by wlproxy).
    let compositor_received = read_packet(&mut compositor).unwrap().unwrap();
    assert_eq!(
        compositor_received.id, 2,
        "compositor should receive bind on registry"
    );
    assert_eq!(
        compositor_received.opcode, 0,
        "compositor should receive bind opcode"
    );
    let mut cursor = std::io::Cursor::new(&compositor_received.body);
    let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
    let iface = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        iface.as_deref(),
        Some("xdg_wm_base"),
        "only xdg_wm_base bind should reach compositor, got {:?}",
        iface
    );

    // 7. Client should receive the sentinel (confirming wlproxy still works normally).
    let sentinel = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(sentinel.id, 1);
    assert_eq!(sentinel.opcode, 1);

    // 8. Verify client→compositor still works for normal messages after blocked bind.
    //    Build object chain on xdg_wm_base to confirm app_id replacement works.
    {
        // XdgWmBase.get_xdg_surface (opcode=2, id 4) → surface at id 5.
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 4,
                opcode: 2,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    {
        // XdgSurface.create_toplevel (opcode=1, id 5) → toplevel at id 6.
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 6).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 5,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_block_interfaces_drops_messages_to_blocked_object() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &["--block", "zwlr_layer_shell_v1"],
        dir.path(),
        &mock_listener,
    );

    // 1. Create registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 2. Client sends bind for zwlr_layer_shell_v1 — blocked by wlproxy.
    //    (No globals announced — the malicious client is guessing type_id.)
    let blocked_obj_id = 3u32;
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "zwlr_layer_shell_v1").unwrap();
        proto::write_arg_uint(&mut body, 5).unwrap();
        proto::write_arg_uint(&mut body, blocked_obj_id).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }

    // 4. Client sends a message to the blocked object (e.g. a get_layer_surface request).
    //    This should also be dropped by wlproxy.
    {
        // zwlr_layer_shell_v1.get_layer_surface (opcode 0 for example).
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 10).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: blocked_obj_id,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    }

    // 5. Also verify that a normal packet to display still works.
    //    Client sends a non-blocked message (Display.get_registry creates another registry
    //    at a different id — just sending any valid-looking packet to id 1, opcode 0).
    {
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 0,
                body: vec![0; 4],
            },
        )
        .unwrap();
    }

    // 6. Compositor should NOT see the blocked bind or the message to blocked object.
    //    Compositor should only see the Display opcode 0 message.
    let received = read_packet(&mut compositor).unwrap().unwrap();
    assert_eq!(
        received.id, 1,
        "only display message should reach compositor"
    );
    assert_eq!(received.opcode, 0);

    cleanup_wlproxy(wlproxy);
}

#[test]
fn wlproxy_block_multiple_flags() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    // Pass --block twice with different values.
    let (wlproxy, mut compositor, mut client) = spawn_wlproxy(
        &[
            "--block",
            "zwlr_layer_shell_v1",
            "--block",
            "ext_data_control_manager_v1",
        ],
        dir.path(),
        &mock_listener,
    );

    // 1. Create registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 2. Compositor sends globals.
    let send_global = |compositor: &mut UnixStream, type_id: u32, name: &str, version: u32| {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, type_id).unwrap();
        proto::write_arg_string(&mut body, name).unwrap();
        proto::write_arg_uint(&mut body, version).unwrap();
        write_packet(
            compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        )
        .unwrap();
    };

    send_global(&mut compositor, 0, "wl_compositor", 4);
    send_global(&mut compositor, 1, "zwlr_layer_shell_v1", 5); // BLOCKED
    send_global(&mut compositor, 2, "ext_data_control_manager_v1", 1); // BLOCKED
    send_global(&mut compositor, 3, "xdg_wm_base", 6);

    // 3. Sentinel.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        write_packet(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        )
        .unwrap();
    }

    // 4. Client should see only wl_compositor and xdg_wm_base.
    for &expected in &["wl_compositor", "xdg_wm_base"] {
        let p = read_packet(&mut client).unwrap().unwrap();
        assert_eq!(p.id, 2);
        assert_eq!(p.opcode, 0);
        let mut cursor = std::io::Cursor::new(&p.body);
        let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
        let name = proto::read_arg_string(&mut cursor).unwrap();
        assert_eq!(
            name.as_deref(),
            Some(expected),
            "unexpected global: got {name:?}, expected {expected}"
        );
    }

    // 5. Sentinel.
    let sentinel = read_packet(&mut client).unwrap().unwrap();
    assert_eq!(sentinel.id, 1);
    assert_eq!(sentinel.opcode, 1);

    cleanup_wlproxy(wlproxy);
}
