use std::os::unix::net::UnixStream;
use std::time::Duration;

use wlproxy::proto::{self, read_packet, write_packet, Packet};
use wlproxy::{handle_connection, Args};

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
// Helpers for library-based wlproxy tests
// ---------------------------------------------------------------------------

fn send_pkt(s: &mut UnixStream, pkt: &Packet) {
    write_packet(s, pkt).unwrap();
}

fn recv_pkt(s: &mut UnixStream) -> Packet {
    read_packet(s).unwrap().unwrap()
}

fn base_args(dir: &std::path::Path) -> Args {
    Args {
        upstream: None,
        app_id: None,
        prefix_app_id: false,
        title: None,
        prefix_title: false,
        block: vec![],
        quiet: true,
        debug: false,
        downstream: dir.join("dummy.sock"),
    }
}

/// Build the full Wayland object chain through handle_connection.
fn build_object_chain(client: &mut UnixStream, compositor: &mut UnixStream) {
    // 1. Display.get_registry (opcode=1) → Registry at id 2.
    {
        let mut b = vec![];
        proto::write_arg_uint(&mut b, 2).unwrap();
        send_pkt(
            client,
            &Packet {
                id: 1,
                opcode: 1,
                body: b,
            },
        );
    }
    let _ = recv_pkt(compositor);

    // 2. Compositor sends Registry.global for xdg_wm_base, type_id=0.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        send_pkt(
            compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    let _ = recv_pkt(client);

    // 3. Client sends Registry.bind → xdg_wm_base at id 3.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        send_pkt(
            client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    let _ = recv_pkt(compositor);

    // 4. XdgWmBase.get_xdg_surface (opcode=2, id 3) → XdgSurface at id 4.
    {
        let mut b = vec![];
        proto::write_arg_uint(&mut b, 4).unwrap();
        send_pkt(
            client,
            &Packet {
                id: 3,
                opcode: 2,
                body: b,
            },
        );
    }
    let _ = recv_pkt(compositor);

    // 5. XdgSurface.create_toplevel (opcode=1, id 4) → XdgToplevel at id 5.
    {
        let mut b = vec![];
        proto::write_arg_uint(&mut b, 5).unwrap();
        send_pkt(
            client,
            &Packet {
                id: 4,
                opcode: 1,
                body: b,
            },
        );
    }
    let _ = recv_pkt(compositor);
}

// ---------------------------------------------------------------------------
// Basic passthrough (end-to-end)
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_basic_passthrough() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = base_args(dir.path());
    handle_connection(downstream, upstream, &args);

    // Send message client → compositor.
    let sent = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAB, 0xCD, 0x00, 0x00],
    };
    send_pkt(&mut client, &sent);
    let received = recv_pkt(&mut compositor);
    assert_eq!(received, sent, "client→compositor passthrough failed");

    // Send message compositor → client.
    let reply = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAA],
    };
    send_pkt(&mut compositor, &reply);
    let received = recv_pkt(&mut client);
    assert_eq!(received, reply, "compositor→client passthrough failed");
}

// ---------------------------------------------------------------------------
// Multiple concurrent connections
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_multiple_concurrent_connections() {
    let dir = tempfile::tempdir().unwrap();

    // Connection 1
    let (mut client1, downstream1) = UnixStream::pair().unwrap();
    let (upstream1, mut compositor1) = UnixStream::pair().unwrap();
    // Connection 2
    let (mut client2, downstream2) = UnixStream::pair().unwrap();
    let (upstream2, mut compositor2) = UnixStream::pair().unwrap();

    for s in [&client1, &client2, &compositor1, &compositor2] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = base_args(dir.path());
    handle_connection(downstream1, upstream1, &args);
    handle_connection(downstream2, upstream2, &args);

    // Send message client1 → compositor1.
    let msg1 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0x11, 0x22, 0x00, 0x00],
    };
    send_pkt(&mut client1, &msg1);
    assert_eq!(recv_pkt(&mut compositor1), msg1, "client1 → compositor1");

    // Send message client2 → compositor2.
    let msg2 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0x33, 0x44, 0x00, 0x00],
    };
    send_pkt(&mut client2, &msg2);
    assert_eq!(recv_pkt(&mut compositor2), msg2, "client2 → compositor2");

    // Send message compositor1 → client1.
    let reply1 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xAA],
    };
    send_pkt(&mut compositor1, &reply1);
    assert_eq!(recv_pkt(&mut client1), reply1, "compositor1 → client1");

    // Send message compositor2 → client2.
    let reply2 = Packet {
        id: 1,
        opcode: 0,
        body: vec![0xBB],
    };
    send_pkt(&mut compositor2, &reply2);
    assert_eq!(recv_pkt(&mut client2), reply2, "compositor2 → client2");
}

// ---------------------------------------------------------------------------
// Object chain + app_id / title tests
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_object_chain_and_app_id_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        app_id: Some("filtered".to_string()),
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    // Client sends XdgToplevel.set_app_id (opcode=3, id 5).
    let mut body = vec![];
    proto::write_arg_string(&mut body, "my-app").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 3,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("filtered"),
        "app_id replacement failed: got {replaced:?}"
    );
}

#[test]
fn wlproxy_title_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        title: Some("filtered-title".to_string()),
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    // Client sends XdgToplevel.set_title (opcode=2, id 5).
    let mut body = vec![];
    proto::write_arg_string(&mut body, "my-title").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 2,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("filtered-title"),
        "title replacement failed: got {replaced:?}"
    );
}

#[test]
fn wlproxy_app_id_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        app_id: Some("pfx-".to_string()),
        prefix_app_id: true,
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    let mut body = vec![];
    proto::write_arg_string(&mut body, "my-app").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 3,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("pfx-my-app"),
        "app_id prefix failed: got {replaced:?}"
    );
}

#[test]
fn wlproxy_empty_app_id() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        app_id: Some("fallback".to_string()),
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    let mut body = vec![];
    proto::write_arg_string(&mut body, "").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 3,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("fallback"),
        "empty app_id should be replaced: got {replaced:?}"
    );
}

#[test]
fn wlproxy_empty_title_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        title: Some("pfx-".to_string()),
        prefix_title: true,
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    let mut body = vec![];
    proto::write_arg_string(&mut body, "").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 2,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("pfx-"),
        "empty title should be prefixed: got {replaced:?}"
    );
}

#[test]
fn wlproxy_title_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        title: Some("pfx-".to_string()),
        prefix_title: true,
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    let mut body = vec![];
    proto::write_arg_string(&mut body, "my-title").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 2,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("pfx-my-title"),
        "title prefix failed: got {replaced:?}"
    );
}

// ---------------------------------------------------------------------------
// Object removal tracking (Display.delete_id)
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_delete_id() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        app_id: Some("filtered".to_string()),
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    // Compositor sends Display.delete_id for obj 5 (XdgToplevel).
    let mut body = vec![];
    proto::write_arg_uint(&mut body, 5).unwrap();
    send_pkt(
        &mut compositor,
        &Packet {
            id: 1,
            opcode: 1,
            body,
        },
    );
    let _ = recv_pkt(&mut client);

    // Client sends set_app_id for obj 5 → passthrough UNMODIFIED
    // because obj 5 is no longer tracked.
    let mut body = vec![];
    proto::write_arg_string(&mut body, "my-app").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 3,
            body,
        },
    );

    let received = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&received.body[..]);
    let app_id = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        app_id.as_deref(),
        Some("my-app"),
        "set_app_id should pass through unmodified after delete_id: got {app_id:?}"
    );
}

#[test]
fn wlproxy_delete_id_registry() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        app_id: Some("filtered".to_string()),
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 1. Client sends Display.get_registry (opcode=1, id 1) → Registry at id 2.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 2. Compositor sends globals on registry (id 2), including xdg_wm_base.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut client);

    // 3. Compositor deletes registry (id 2).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut client);

    // 4. Client sends get_registry again → new Registry at id 3.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 3).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 5. Compositor sends global on new registry (id 3) for wl_compositor.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 3,
                opcode: 0,
                body,
            },
        );
    }
    let p = recv_pkt(&mut client);
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
}

// ---------------------------------------------------------------------------
// Global event filtering — only xdg_wm_base objects are tracked
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_global_filtering() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        app_id: Some("filtered".to_string()),
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 0. Display.get_registry → Registry at id 2.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 1. Global for wl_compositor (NOT xdg_wm_base, type_id=0).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut client);

    // 2. Global for xdg_wm_base (type_id=1).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut client);

    // 3. Client binds wl_compositor (type_id=0) → obj 3 (NOT tracked).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 4. Client binds xdg_wm_base (type_id=1) → obj 4 (SHOULD be tracked).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 5. XdgWmBase.get_xdg_surface (opcode=2, id 4) → surface at id 5.
    {
        let mut b = vec![];
        proto::write_arg_uint(&mut b, 5).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 4,
                opcode: 2,
                body: b,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 6. XdgSurface.create_toplevel (opcode=1, id 5) → toplevel at id 6.
    {
        let mut b = vec![];
        proto::write_arg_uint(&mut b, 6).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 5,
                opcode: 1,
                body: b,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 7. set_app_id on obj 3 (NOT tracked) → passthrough UNMODIFIED.
    {
        let mut body = vec![];
        proto::write_arg_string(&mut body, "my-app-3").unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 3,
                opcode: 3,
                body,
            },
        );
    }
    {
        let received = recv_pkt(&mut compositor);
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
        send_pkt(
            &mut client,
            &Packet {
                id: 6,
                opcode: 3,
                body,
            },
        );
    }
    {
        let received = recv_pkt(&mut compositor);
        let mut cursor = std::io::Cursor::new(&received.body[..]);
        let app_id = proto::read_arg_string(&mut cursor).unwrap();
        assert_eq!(
            app_id.as_deref(),
            Some("filtered"),
            "obj 6 (XdgToplevel) should have app_id replaced: got {app_id:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// FD forwarding in server→client direction
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_fd_forwarding_server_to_client() {
    use std::io::Read;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use uds::UnixStreamExt;

    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, compositor) = UnixStream::pair().unwrap();

    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    let args = base_args(dir.path());
    handle_connection(downstream, upstream, &args);

    // Create a dummy socket pair — one end's FD is sent through wlproxy.
    let (dummy_send, dummy_recv) = UnixStream::pair().unwrap();
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
}

#[test]
fn wlproxy_fd_forwarding_client_to_server() {
    use std::io::Read;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use uds::UnixStreamExt;

    let dir = tempfile::tempdir().unwrap();
    let (client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    compositor
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    let args = base_args(dir.path());
    handle_connection(downstream, upstream, &args);

    // Create a dummy socket pair — one end's FD is sent through wlproxy.
    let (dummy_send, dummy_recv) = UnixStream::pair().unwrap();
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
}

// ---------------------------------------------------------------------------
// Interface blocking
// ---------------------------------------------------------------------------

#[test]
fn wlproxy_block_interfaces() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        block: vec![
            "zwlr_layer_shell_v1".to_string(),
            "ext_data_control_manager_v1".to_string(),
            "zwlr_screencopy_manager_v1".to_string(),
        ],
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 1. Create registry via get_registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 2. Helper to send a global event.
    let send_global = |compositor: &mut UnixStream, type_id: u32, name: &str, version: u32| {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, type_id).unwrap();
        proto::write_arg_string(&mut body, name).unwrap();
        proto::write_arg_uint(&mut body, version).unwrap();
        send_pkt(
            compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    };

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
        send_pkt(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }

    // 4. Client reads globals — should see only wl_compositor, xdg_wm_base, wl_shm.
    for &expected in &["wl_compositor", "xdg_wm_base", "wl_shm"] {
        let p = recv_pkt(&mut client);
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

    // 5. Next packet should be the sentinel.
    let sentinel = recv_pkt(&mut client);
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
}

#[test]
fn wlproxy_block_interfaces_app_id_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        app_id: Some("filtered".to_string()),
        block: vec![
            "zwlr_layer_shell_v1".to_string(),
            "ext_data_control_manager_v1".to_string(),
        ],
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    // Send set_app_id and verify it's replaced.
    let mut body = vec![];
    proto::write_arg_string(&mut body, "my-app").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 3,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("filtered"),
        "app_id replacement should work alongside interface blocking: got {replaced:?}"
    );
}

#[test]
fn wlproxy_block_interfaces_does_not_leak_fds() {
    use std::os::unix::io::AsRawFd;
    use uds::UnixStreamExt;

    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        block: vec!["zwlr_layer_shell_v1".to_string()],
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 1. Create registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

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

    let (dummy_send, _dummy_recv) = UnixStream::pair().unwrap();
    let send_fd = dummy_send.as_raw_fd();
    compositor.send_fds(&packet_bytes, &[send_fd]).unwrap();
    drop(dummy_send);

    // 3. Send a global for a non-blocked interface.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }

    // 4. Send a sentinel.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }

    // 5. Client reads — should see wl_compositor.
    let p = recv_pkt(&mut client);
    assert_eq!(p.id, 2);
    assert_eq!(p.opcode, 0);
    let mut cursor = std::io::Cursor::new(&p.body);
    let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
    let name = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(name.as_deref(), Some("wl_compositor"));

    // 6. Verify sentinel.
    let sentinel = recv_pkt(&mut client);
    assert_eq!(sentinel.id, 1);
    assert_eq!(sentinel.opcode, 1);
    let mut cursor = std::io::Cursor::new(&sentinel.body);
    assert_eq!(proto::read_arg_uint(&mut cursor).unwrap(), 999);
}

#[test]
fn wlproxy_block_interfaces_does_not_leak_fds_on_bind() {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use uds::UnixStreamExt;

    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        block: vec!["zwlr_layer_shell_v1".to_string()],
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 1. Create registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 2. Build a bind request for the blocked interface.
    let mut bind_body = vec![];
    proto::write_arg_uint(&mut bind_body, 0).unwrap();
    proto::write_arg_string(&mut bind_body, "zwlr_layer_shell_v1").unwrap();
    proto::write_arg_uint(&mut bind_body, 5).unwrap();
    proto::write_arg_uint(&mut bind_body, 3).unwrap();
    let mut packet_bytes = vec![];
    write_packet(
        &mut packet_bytes,
        &Packet {
            id: 2,
            opcode: 0,
            body: bind_body,
        },
    )
    .unwrap();

    // 3. Send the bind request with an FD attached.
    let (dummy_send, mut dummy_recv) = UnixStream::pair().unwrap();
    let send_fd = dummy_send.as_raw_fd();
    client.send_fds(&packet_bytes, &[send_fd]).unwrap();
    drop(dummy_send);

    // 4. Send a non-blocked message.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 0,
                body,
            },
        );
    }

    // 5. Compositor receives the non-blocked message (NOT the blocked bind).
    let received = recv_pkt(&mut compositor);
    assert_eq!(
        received.id, 1,
        "only non-blocked message should reach compositor"
    );
    assert_eq!(received.opcode, 0);

    // 6. Verify the FD was closed.
    let mut probe = [0u8; 1];
    let n = dummy_recv.read(&mut probe).unwrap();
    assert_eq!(n, 0, "dummy FD should have been closed by wlproxy");

    drop(dummy_recv);
}

#[test]
fn wlproxy_block_interfaces_and_title_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        title: Some("pfx-".to_string()),
        prefix_title: true,
        block: vec!["zwlr_layer_shell_v1".to_string()],
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    build_object_chain(&mut client, &mut compositor);

    // Send set_title and verify it's prefixed.
    let mut body = vec![];
    proto::write_arg_string(&mut body, "my-title").unwrap();
    send_pkt(
        &mut client,
        &Packet {
            id: 5,
            opcode: 2,
            body,
        },
    );

    let modified = recv_pkt(&mut compositor);
    let mut cursor = std::io::Cursor::new(&modified.body[..]);
    let replaced = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        replaced.as_deref(),
        Some("pfx-my-title"),
        "title prefix should work alongside interface blocking: got {replaced:?}"
    );
}

#[test]
fn wlproxy_block_interfaces_blocks_bind_requests() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        block: vec!["zwlr_layer_shell_v1".to_string()],
        app_id: Some("filtered".to_string()),
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 1. Create registry via get_registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 2. Compositor announces globals: xdg_wm_base and zwlr_layer_shell_v1.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "zwlr_layer_shell_v1").unwrap();
        proto::write_arg_uint(&mut body, 5).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }
    // Client reads globals (second is blocked, only 1 arrives).
    let global = recv_pkt(&mut client);
    assert_eq!(global.id, 2);
    assert_eq!(global.opcode, 0);
    let mut cursor = std::io::Cursor::new(&global.body);
    let _tid = proto::read_arg_uint(&mut cursor).unwrap();
    let name = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        name.as_deref(),
        Some("xdg_wm_base"),
        "only xdg_wm_base global should reach client"
    );

    // 3. Client tries to bind zwlr_layer_shell_v1 (blocked).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "zwlr_layer_shell_v1").unwrap();
        proto::write_arg_uint(&mut body, 5).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }

    // 4. Client binds xdg_wm_base (should pass through).
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 6).unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }

    // 5. Sentinel from compositor.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }

    // 6. Compositor should receive only the xdg_wm_base bind.
    let compositor_received = recv_pkt(&mut compositor);
    assert_eq!(compositor_received.id, 2);
    assert_eq!(compositor_received.opcode, 0);
    let mut cursor = std::io::Cursor::new(&compositor_received.body);
    let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
    let iface = proto::read_arg_string(&mut cursor).unwrap();
    assert_eq!(
        iface.as_deref(),
        Some("xdg_wm_base"),
        "only xdg_wm_base bind should reach compositor, got {:?}",
        iface
    );

    // 7. Client receives sentinel.
    let sentinel = recv_pkt(&mut client);
    assert_eq!(sentinel.id, 1);
    assert_eq!(sentinel.opcode, 1);

    // 8. Build object chain on xdg_wm_base.
    {
        let mut b = vec![];
        proto::write_arg_uint(&mut b, 5).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 4,
                opcode: 2,
                body: b,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    {
        let mut b = vec![];
        proto::write_arg_uint(&mut b, 6).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 5,
                opcode: 1,
                body: b,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);
}

#[test]
fn wlproxy_block_interfaces_drops_messages_to_blocked_object() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        block: vec!["zwlr_layer_shell_v1".to_string()],
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 1. Create registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 2. Client sends bind for zwlr_layer_shell_v1 — blocked.
    let blocked_obj_id = 3u32;
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "zwlr_layer_shell_v1").unwrap();
        proto::write_arg_uint(&mut body, 5).unwrap();
        proto::write_arg_uint(&mut body, blocked_obj_id).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    }

    // 3. Client sends a message to the blocked object.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 10).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: blocked_obj_id,
                opcode: 0,
                body,
            },
        );
    }

    // 4. Send a normal packet to display.
    {
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 0,
                body: vec![0; 4],
            },
        );
    }

    // 5. Compositor should only see the Display message.
    let received = recv_pkt(&mut compositor);
    assert_eq!(
        received.id, 1,
        "only display message should reach compositor"
    );
    assert_eq!(received.opcode, 0);
}

#[test]
fn wlproxy_block_multiple_flags() {
    let dir = tempfile::tempdir().unwrap();
    let (mut client, downstream) = UnixStream::pair().unwrap();
    let (upstream, mut compositor) = UnixStream::pair().unwrap();

    for s in [&client, &compositor] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    let args = Args {
        block: vec![
            "zwlr_layer_shell_v1".to_string(),
            "ext_data_control_manager_v1".to_string(),
        ],
        ..base_args(dir.path())
    };
    handle_connection(downstream, upstream, &args);

    // 1. Create registry.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &mut client,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }
    let _ = recv_pkt(&mut compositor);

    // 2. Helper to send globals.
    let send_global = |compositor: &mut UnixStream, type_id: u32, name: &str, version: u32| {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, type_id).unwrap();
        proto::write_arg_string(&mut body, name).unwrap();
        proto::write_arg_uint(&mut body, version).unwrap();
        send_pkt(
            compositor,
            &Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );
    };

    send_global(&mut compositor, 0, "wl_compositor", 4);
    send_global(&mut compositor, 1, "zwlr_layer_shell_v1", 5); // BLOCKED
    send_global(&mut compositor, 2, "ext_data_control_manager_v1", 1); // BLOCKED
    send_global(&mut compositor, 3, "xdg_wm_base", 6);

    // 3. Sentinel.
    {
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 999).unwrap();
        send_pkt(
            &mut compositor,
            &Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );
    }

    // 4. Client should see only wl_compositor and xdg_wm_base.
    for &expected in &["wl_compositor", "xdg_wm_base"] {
        let p = recv_pkt(&mut client);
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
    let sentinel = recv_pkt(&mut client);
    assert_eq!(sentinel.id, 1);
    assert_eq!(sentinel.opcode, 1);
}
