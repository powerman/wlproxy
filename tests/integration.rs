use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use filterway::proto::{self, read_packet, write_packet, Packet};
use tempfile::tempdir;

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
// Filterway binary passthrough (end-to-end)
// ---------------------------------------------------------------------------

fn filterway_binary() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_filterway")
        .map(PathBuf::from)
        .expect("filterway binary not found — run `cargo test` (CARGO_BIN_EXE_filterway is always set by cargo test)")
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
fn filterway_basic_passthrough() {
    let dir = tempdir().unwrap();
    let upstream = dir.path().join("upstream.sock");
    let downstream = dir.path().join("downstream.sock");

    // Start mock compositor (listener for filterway's upstream connection).
    let mock_listener = std::os::unix::net::UnixListener::bind(&upstream).unwrap();

    // Launch filterway.
    let mut filterway = Command::new(filterway_binary())
        .args([
            "--upstream",
            upstream.to_str().unwrap(),
            "--downstream",
            downstream.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start filterway");

    // Connect as a client to filterway's downstream socket.
    let mut client = connect_with_retry(&downstream, Duration::from_secs(5));

    // Mock compositor accepts filterway's connection.
    let (mut compositor, _) = mock_listener.accept().unwrap();

    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    compositor
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Send message client → filterway → compositor.
    let sent = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAB, 0xCD, 0x00, 0x00],
    };
    write_packet(&mut client, &sent).unwrap();
    let received = read_packet(&mut compositor).unwrap().unwrap();
    assert_eq!(received, sent, "client→compositor passthrough failed");

    // Send message compositor → filterway → client.
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
    filterway.kill().unwrap();
    let output = filterway.wait_with_output().unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("filterway stderr:\n{stderr}");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers for filterway end-to-end tests
// ---------------------------------------------------------------------------

/// Spawn filterway with the given extra args (beyond --upstream/--downstream)
/// and accept its upstream connection against the mock listener.
/// Returns (filterway_child, compositor_stream, client_stream).
fn spawn_filterway(
    extra_args: &[&str],
    dir: &std::path::Path,
    mock_listener: &UnixListener,
) -> (std::process::Child, UnixStream, UnixStream) {
    let upstream = dir.join("upstream.sock");
    let downstream = dir.join("downstream.sock");

    let filterway = Command::new(filterway_binary())
        .args([
            "--upstream",
            upstream.to_str().unwrap(),
            "--downstream",
            downstream.to_str().unwrap(),
        ])
        .args(extra_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start filterway");

    let client = connect_with_retry(&downstream, Duration::from_secs(5));

    let (compositor, _) = mock_listener.accept().unwrap();

    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    compositor
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    (filterway, compositor, client)
}

/// Send the standard Wayland object-chain messages that make filterway
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

/// Kill the filterway child and print stderr if the exit code is non-zero.
fn cleanup_filterway(mut filterway: std::process::Child) {
    filterway.kill().unwrap();
    let output = filterway.wait_with_output().unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("filterway stderr:\n{stderr}");
        }
    }
}

#[test]
fn filterway_object_chain_and_app_id_replacement() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (filterway, mut compositor, mut client) =
        spawn_filterway(&["--app-id", "filtered"], dir.path(), &mock_listener);

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

    cleanup_filterway(filterway);
}

#[test]
fn filterway_title_replacement() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (filterway, mut compositor, mut client) =
        spawn_filterway(&["--title", "filtered-title"], dir.path(), &mock_listener);

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

    cleanup_filterway(filterway);
}

#[test]
fn filterway_app_id_prefix() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (filterway, mut compositor, mut client) = spawn_filterway(
        &["--app-id", "pfx-", "--prefix"],
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

    cleanup_filterway(filterway);
}

#[test]
fn filterway_title_prefix() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (filterway, mut compositor, mut client) = spawn_filterway(
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

    cleanup_filterway(filterway);
}

// ---------------------------------------------------------------------------
// Object removal tracking (Display.delete_id)
// ---------------------------------------------------------------------------

#[test]
fn filterway_delete_id() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (filterway, mut compositor, mut client) =
        spawn_filterway(&["--app-id", "filtered"], dir.path(), &mock_listener);

    build_object_chain(&mut client, &mut compositor);

    // Compositor sends Display.delete_id for obj 5 (XdgToplevel).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        write_packet(&mut compositor, &Packet { id: 1, opcode: 1, body }).unwrap();
    }
    // Client drains the forwarded delete_id (server→client thread removes obj 5 first).
    let _ = read_packet(&mut client).unwrap().unwrap();

    // Client sends set_app_id for obj 5 → should passthrough UNMODIFIED
    // because obj 5 is no longer tracked.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app").unwrap();
        write_packet(&mut client, &Packet { id: 5, opcode: 3, body }).unwrap();
    }
    let received = read_packet(&mut compositor).unwrap().unwrap();
    let mut cursor = std::io::Cursor::new(&received.body[..]);
    let app_id = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        app_id.as_deref(),
        Some("my-app"),
        "set_app_id should pass through unmodified after delete_id: got {app_id:?}"
    );

    cleanup_filterway(filterway);
}

// ---------------------------------------------------------------------------
// Global event filtering — only xdg_wm_base objects are tracked
// ---------------------------------------------------------------------------

#[test]
fn filterway_global_filtering() {
    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (filterway, mut compositor, mut client) =
        spawn_filterway(&["--app-id", "filtered"], dir.path(), &mock_listener);

    // ---- Custom object chain with multiple globals ----

    // 0. Client sends Display.get_registry (opcode=1, id 1) → Registry at id 2.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        write_packet(&mut client, &Packet { id: 1, opcode: 1, body }).unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 1. Compositor sends global for wl_compositor (NOT xdg_wm_base, type_id=0).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(&mut compositor, &Packet { id: 2, opcode: 0, body }).unwrap();
    }
    let _ = read_packet(&mut client).unwrap().unwrap();

    // 2. Compositor sends global for xdg_wm_base (type_id=1).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        write_packet(&mut compositor, &Packet { id: 2, opcode: 0, body }).unwrap();
    }
    let _ = read_packet(&mut client).unwrap().unwrap();

    // 3. Client binds wl_compositor (type_id=0) → obj 3 (NOT tracked, wrong type_id).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        write_packet(&mut client, &Packet { id: 2, opcode: 0, body }).unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 4. Client binds xdg_wm_base (type_id=1) → obj 4 (SHOULD be tracked).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        write_packet(&mut client, &Packet { id: 2, opcode: 0, body }).unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 5. XdgWmBase.get_xdg_surface (opcode=2, id 4) → surface at id 5.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        write_packet(&mut client, &Packet { id: 4, opcode: 2, body }).unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 6. XdgSurface.create_toplevel (opcode=1, id 5) → toplevel at id 6.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 6).unwrap();
        write_packet(&mut client, &Packet { id: 5, opcode: 1, body }).unwrap();
    }
    let _ = read_packet(&mut compositor).unwrap().unwrap();

    // 7. set_app_id on obj 3 (NOT tracked) → passthrough UNMODIFIED.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app-3").unwrap();
        write_packet(&mut client, &Packet { id: 3, opcode: 3, body }).unwrap();
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
        write_packet(&mut client, &Packet { id: 6, opcode: 3, body }).unwrap();
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

    cleanup_filterway(filterway);
}

// ---------------------------------------------------------------------------
// FD forwarding in server→client direction
// ---------------------------------------------------------------------------

#[test]
fn filterway_fd_forwarding_server_to_client() {
    use std::io::Read;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use uds::UnixStreamExt;

    let dir = tempdir().unwrap();
    let mock_listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("upstream.sock")).unwrap();

    let (filterway, compositor, mut client) =
        spawn_filterway(&[], dir.path(), &mock_listener);

    // Create a dummy socket pair — one end's FD is sent through filterway.
    let (dummy_send, dummy_recv) = std::os::unix::net::UnixStream::pair().unwrap();
    let send_fd = dummy_send.as_raw_fd();

    // Build a minimal valid Wayland packet (empty body → 8 bytes total).
    let mut packet_bytes = vec![];
    write_packet(&mut packet_bytes, &Packet { id: 1, opcode: 0, body: vec![] }).unwrap();
    assert_eq!(packet_bytes.len(), 8);

    // Compositor sends the packet with an FD attached.
    compositor.send_fds(&packet_bytes, &[send_fd]).unwrap();
    drop(dummy_send);

    // Client reads from downstream (via filterway).
    // filterway should forward both data and the FD.
    // AncillaryWriter sends header_word1 (4 bytes + FD) and header_word2 (4 bytes)
    // as separate write() calls — recv_fds may return only the first chunk.
    let mut buf = [0u8; 8];
    let mut fd_buf = [0i32; 8];
    let (n, nfds) = client.recv_fds(&mut buf, &mut fd_buf).unwrap();
    assert!(nfds == 1, "FD should be forwarded by filterway");
    assert!(fd_buf[0] > 0, "received FD should be valid");
    if n < 8 {
        client.read_exact(&mut buf[n..]).unwrap();
    }

    // Parse and verify the packet content.
    let mut cursor = std::io::Cursor::new(&buf[..]);
    let received = read_packet(&mut cursor).unwrap().unwrap();
    assert_eq!(received, Packet { id: 1, opcode: 0, body: vec![] });

    // Close the received FD.
    drop(unsafe { OwnedFd::from_raw_fd(fd_buf[0]) });
    drop(dummy_recv);

    cleanup_filterway(filterway);
}
