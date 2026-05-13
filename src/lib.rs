pub mod proto;

use {
    clap::Parser,
    rustix::{
        fd::{AsFd, FromRawFd, OwnedFd, RawFd},
        fs::{flock, OpenOptionsExt},
    },
    sd_notify::NotifyState,
    std::{
        collections::{HashMap, HashSet},
        fmt::Display,
        fs::{remove_file, File},
        io::Cursor,
        os::unix::net::{UnixListener, UnixStream},
        path::PathBuf,
        sync::{Arc, Mutex, OnceLock},
        thread::spawn,
    },
};

use crate::proto::read_arg_string;
use std::sync::atomic::{AtomicBool, Ordering};
use uds::UnixStreamExt;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ObjType {
    Display,
    Registry,
    XdgWmBase { ver: u32 },
    XdgSurface { ver: u32 },
    XdgToplevel { ver: u32 },
}

#[derive(Parser, Clone)]
#[command(name = "wlproxy")]
pub struct Args {
    /// Full path to compositor Wayland socket.
    #[arg(short = 'u', long = "upstream")]
    pub upstream: Option<PathBuf>,
    /// Force all xdg toplevels to have the same app id
    #[arg(short = 'a', long = "app-id")]
    pub app_id: Option<String>,
    /// Prefix the app id instead of replacing
    #[arg(short = 'A', long = "prefix-app-id")]
    pub prefix_app_id: bool,
    /// Force all xdg toplevels to have the same title
    #[arg(short = 't', long = "title")]
    pub title: Option<String>,
    /// Prefix the title instead of replacing
    #[arg(short = 'T', long = "prefix-title")]
    pub prefix_title: bool,
    /// Wayland interfaces to block (can be specified multiple times)
    #[arg(short = 'b', long = "block", value_delimiter = ',')]
    pub block: Vec<String>,
    /// Suppress warnings about unknown interface names
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Print debug messages
    #[arg(long = "debug")]
    pub debug: bool,
    /// Full path for the new Wayland socket
    pub downstream: PathBuf,
}

fn default_upstream() -> PathBuf {
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set for Wayland");
    let socket_name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    PathBuf::from(runtime_dir).join(socket_name)
}

fn known_protocols() -> &'static [&'static str] {
    static LIST: OnceLock<Vec<&'static str>> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut list: Vec<&'static str> = include_str!("../known_protocols.txt")
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .collect();
        list.sort();
        list.dedup();
        list
    })
}

trait Errorize<T> {
    fn context(self, text: &str) -> Result<T, String>;
}

impl<T, E: Display> Errorize<T> for Result<T, E> {
    fn context(self, text: &str) -> Result<T, String> {
        match self {
            Ok(x) => Ok(x),
            Err(e) => Err(format!("{}: {}", text, e)),
        }
    }
}

fn is_blocked_interface(name: &str, block: &[String]) -> bool {
    block.iter().any(|b| b == name)
}

fn validate_interfaces(block: &[String], quiet: bool) {
    if quiet || block.is_empty() {
        return;
    }
    for name in block {
        if !known_protocols().contains(&name.as_str()) {
            eprintln!(
                "Warning: unknown Wayland interface \"{}\" in --block list",
                name
            );
        }
    }
}

struct ListenerContext {
    _filelock: File,
    listener: UnixListener,
    lock_path: PathBuf,
    downstream: PathBuf,
}

fn setup_listener(args: &Args) -> Result<ListenerContext, String> {
    let lock_path = args.downstream.with_extension("lock");
    let filelock = File::options()
        .mode(0o660)
        .write(true)
        .create(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&lock_path)
        .context("Error opening lock file")?;
    flock(
            filelock.as_fd(),
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .context(
            "Error getting exclusive lock for downstream listener, is another compositor already listening?",
        )?;
    _ = remove_file(&args.downstream);
    let listener =
        UnixListener::bind(&args.downstream).context("Error creating downstream listener")?;
    Ok(ListenerContext {
        _filelock: filelock,
        listener,
        lock_path,
        downstream: args.downstream.clone(),
    })
}

fn drop_fds(fds: &mut Vec<RawFd>) {
    for fd in fds.drain(..) {
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
}

struct AncillaryReader<'a> {
    reader: &'a UnixStream,
    fds: &'a mut Vec<RawFd>,
}

impl<'a> std::io::Read for AncillaryReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut fd_buf = [0i32; 8];
        let (n, nfds) = self.reader.recv_fds(buf, &mut fd_buf)?;
        self.fds.extend(&fd_buf[..nfds]);
        Ok(n)
    }
}

struct AncillaryWriter<'a> {
    writer: &'a UnixStream,
    fds: Vec<RawFd>,
}

impl<'a> AncillaryWriter<'a> {
    fn new(writer: &'a UnixStream, fds: &[RawFd]) -> Self {
        Self {
            writer,
            fds: fds.to_vec(),
        }
    }
}

impl<'a> std::io::Write for AncillaryWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let fds = std::mem::take(&mut self.fds);
        if fds.is_empty() {
            self.writer.write(buf)
        } else {
            self.writer.send_fds(buf, &fds)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

fn handle_client_to_server(
    downstream: &UnixStream,
    upstream: &UnixStream,
    objects: &Arc<Mutex<HashMap<u32, ObjType>>>,
    xdgwmbase_type_id: &Arc<Mutex<Option<(u32, u32)>>>,
    blocked_objects: &Arc<Mutex<HashSet<u32>>>,
    args: &Args,
) -> Result<(), String> {
    let mut ancillary_accum = vec![];
    while let Some(mut packet) = proto::read_packet(&mut AncillaryReader {
        reader: downstream,
        fds: &mut ancillary_accum,
    })
    .context("Error reading message")?
    {
        let should_block = {
            let mut objects = objects.lock().unwrap();
            let mut blocked = blocked_objects.lock().unwrap();

            if blocked.contains(&packet.id) {
                true
            } else {
                let o = objects.get(&packet.id).cloned();
                if args.debug {
                    eprintln!(
                        "Received packet from downstream for tracked object {:?} with {} ancillary FDs: {:?}",
                        o,
                        ancillary_accum.len(),
                        packet
                    );
                }
                if let Some(o) = o {
                    match o {
                        ObjType::Display => {
                            if packet.opcode == 1 {
                                let obj_id = proto::read_arg_uint(&mut Cursor::new(&packet.body))
                                    .context("Error reading registry id")?;
                                objects.insert(obj_id, ObjType::Registry);
                            }
                            false
                        }
                        ObjType::Registry => {
                            if packet.opcode == 0 {
                                let mut cursor = Cursor::new(&packet.body);
                                let obj_type_id = proto::read_arg_uint(&mut cursor)
                                    .context("Error/eof reading bind object type id")?;
                                let interface_name = proto::read_arg_string(&mut cursor)
                                    .context("Error reading bind message type string")?;
                                let version = proto::read_arg_uint(&mut cursor)
                                    .context("Error reading bind message version")?;
                                let obj_id = proto::read_arg_uint(&mut cursor)
                                    .context("Error/eof reading bind object id")?;

                                if let Some(ref name) = interface_name {
                                    if is_blocked_interface(name, &args.block) {
                                        if args.debug {
                                            eprintln!("Blocked bind for interface: {}", name);
                                        }
                                        blocked.insert(obj_id);
                                        true
                                    } else if let Some((want_type_id, _version)) =
                                        *xdgwmbase_type_id.lock().unwrap()
                                    {
                                        if obj_type_id == want_type_id {
                                            objects.insert(
                                                obj_id,
                                                ObjType::XdgWmBase { ver: version },
                                            );
                                        }
                                        false
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        }
                        ObjType::XdgWmBase { ver } => match ver {
                            0..=6 => {
                                if packet.opcode == 2 {
                                    let obj_id =
                                        proto::read_arg_uint(&mut Cursor::new(&packet.body))
                                            .context(
                                                "Error reading xdg wm base create surface id",
                                            )?;
                                    objects.insert(obj_id, ObjType::XdgSurface { ver });
                                }
                                false
                            }
                            _ => {
                                return Err(
                                    format!("Unsupported xdg_wm_base object version {ver}",),
                                )
                            }
                        },
                        ObjType::XdgSurface { ver } => match ver {
                            0..=6 => {
                                if packet.opcode == 1 {
                                    let obj_id =
                                        proto::read_arg_uint(&mut Cursor::new(&packet.body))
                                            .context(
                                                "Error reading xdg surface create toplevel id",
                                            )?;
                                    objects.insert(obj_id, ObjType::XdgToplevel { ver });
                                }
                                false
                            }
                            _ => {
                                return Err(
                                    format!("Unsupported xdg_surface object version {ver}",),
                                )
                            }
                        },
                        ObjType::XdgToplevel { ver } => match ver {
                            0..=6 => {
                                match packet.opcode {
                                    2 => {
                                        if let Some(title) = &args.title {
                                            let read_title =
                                                read_arg_string(&mut packet.body.as_slice())
                                                    .context("Error reading app id message body")?;
                                            packet.body.clear();
                                            let new_title = if args.prefix_title {
                                                format!(
                                                    "{}{}",
                                                    title,
                                                    read_title.unwrap_or_default()
                                                )
                                            } else {
                                                title.clone()
                                            };
                                            proto::write_arg_string(&mut packet.body, &new_title)
                                                .unwrap();
                                            if args.debug {
                                                eprintln!(
                                                    "Modified title; new message: {:?}",
                                                    packet
                                                );
                                            }
                                        }
                                    }
                                    3 => {
                                        if let Some(app_id) = &args.app_id {
                                            let read_app_id =
                                                read_arg_string(&mut packet.body.as_slice())
                                                    .context("Error reading app id message body")?;
                                            packet.body.clear();
                                            let new_app_id = if args.prefix_app_id {
                                                format!(
                                                    "{}{}",
                                                    app_id,
                                                    read_app_id.unwrap_or_default()
                                                )
                                            } else {
                                                app_id.clone()
                                            };
                                            proto::write_arg_string(&mut packet.body, &new_app_id)
                                                .unwrap();
                                            if args.debug {
                                                eprintln!(
                                                    "Modified app id; new message: {:?}",
                                                    packet
                                                );
                                            }
                                        }
                                    }
                                    _ => (),
                                };
                                false
                            }
                            _ => {
                                return Err(format!(
                                    "Unsupported xdg_toplevel object version {ver}",
                                ))
                            }
                        },
                    }
                } else {
                    false
                }
            }
        };

        if should_block {
            drop_fds(&mut ancillary_accum);
            continue;
        }

        proto::write_packet(
            &mut AncillaryWriter::new(upstream, &ancillary_accum),
            &packet,
        )
        .context("Error writing message")?;
        drop_fds(&mut ancillary_accum);
    }
    Ok(())
}

fn handle_server_to_client(
    upstream: &UnixStream,
    downstream: &UnixStream,
    objects: &Arc<Mutex<HashMap<u32, ObjType>>>,
    xdgwmbase_type_id: &Arc<Mutex<Option<(u32, u32)>>>,
    args: &Args,
) -> Result<(), String> {
    let mut ancillary_accum = vec![];
    let mut cache_reg_id = None;
    while let Some(packet) = proto::read_packet(&mut AncillaryReader {
        reader: upstream,
        fds: &mut ancillary_accum,
    })
    .context("Error reading message")?
    {
        if args.debug {
            eprintln!(
                "Received packet from upstream with {} ancillary FDs: {:?}",
                ancillary_accum.len(),
                packet
            );
        }

        if (packet.id, packet.opcode) == (1, 1) {
            let mut cursor = Cursor::new(&packet.body);
            let obj_id =
                proto::read_arg_uint(&mut cursor).context("Error reading display delete obj id")?;
            objects.lock().unwrap().remove(&obj_id);
            if cache_reg_id == Some(obj_id) {
                cache_reg_id = None;
            }
        }
        if let Some(reg_id) = match &cache_reg_id {
            Some(r) => Some(*r),
            None => {
                if let Some(ObjType::Registry) = objects.lock().unwrap().get(&packet.id) {
                    cache_reg_id = Some(packet.id);
                    Some(packet.id)
                } else {
                    None
                }
            }
        } {
            if reg_id == packet.id && packet.opcode == 0 {
                let mut cursor = Cursor::new(&packet.body);
                let type_id = proto::read_arg_uint(&mut cursor)
                    .context("Error reading global message type id")?;
                let type_str = proto::read_arg_string(&mut cursor)
                    .context("Error reading global message type string")?;
                let version = proto::read_arg_uint(&mut cursor)
                    .context("Error reading global message version")?;
                if type_str.as_deref() == Some("xdg_wm_base") {
                    *xdgwmbase_type_id.lock().unwrap() = Some((type_id, version));
                }

                if let Some(ref name) = type_str {
                    if !args.block.is_empty() && args.block.iter().any(|b| b == name) {
                        if args.debug {
                            eprintln!("Blocked global: {}", name);
                        }
                        drop_fds(&mut ancillary_accum);
                        continue;
                    }
                }
            }
        }

        proto::write_packet(
            &mut AncillaryWriter::new(downstream, &ancillary_accum),
            &packet,
        )
        .context("Error writing message")?;
        drop_fds(&mut ancillary_accum);
    }
    Ok(())
}

pub fn handle_connection(downstream: UnixStream, upstream: UnixStream, args: &Args) {
    let objects = Arc::new(Mutex::new(HashMap::new()));
    objects.lock().unwrap().insert(1, ObjType::Display);
    let xdgwmbase_type_id = Arc::new(Mutex::new(None));
    let blocked_objects: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    spawn({
        let downstream = downstream.try_clone().unwrap();
        let upstream = upstream.try_clone().unwrap();
        let objects = objects.clone();
        let xdgwmbase_type_id = xdgwmbase_type_id.clone();
        let blocked_objects = blocked_objects.clone();
        let args = args.clone();
        move || {
            let _defer = defer::defer({
                let downstream = downstream.try_clone().unwrap();
                let upstream = upstream.try_clone().unwrap();
                move || {
                    _ = downstream.shutdown(std::net::Shutdown::Both);
                    _ = upstream.shutdown(std::net::Shutdown::Both);
                }
            });
            if let Err(e) = handle_client_to_server(
                &downstream,
                &upstream,
                &objects,
                &xdgwmbase_type_id,
                &blocked_objects,
                &args,
            ) {
                eprintln!("Warning, client->server thread exiting with error: {}", e);
            }
        }
    });
    spawn({
        let downstream = downstream.try_clone().unwrap();
        let upstream = upstream.try_clone().unwrap();
        let objects = objects.clone();
        let xdgwmbase_type_id = xdgwmbase_type_id.clone();
        let args = args.clone();
        move || {
            let _defer = defer::defer({
                let downstream = downstream.try_clone().unwrap();
                let upstream = upstream.try_clone().unwrap();
                move || {
                    _ = downstream.shutdown(std::net::Shutdown::Both);
                    _ = upstream.shutdown(std::net::Shutdown::Both);
                }
            });
            if let Err(e) =
                handle_server_to_client(&upstream, &downstream, &objects, &xdgwmbase_type_id, &args)
            {
                eprintln!("Warning, server->client thread exiting with error: {}", e);
            }
        }
    });
}

pub fn run(args: Args) -> Result<(), String> {
    run_impl(args, || false)
}

fn run_impl(args: Args, stop: impl Fn() -> bool) -> Result<(), String> {
    validate_interfaces(&args.block, args.quiet);

    let ctx = setup_listener(&args)?;
    let _defer = defer::defer(|| {
        _ = remove_file(&ctx.lock_path);
    });
    let _defer1 = defer::defer(|| {
        _ = remove_file(&ctx.downstream);
    });

    // If the system booted with systemd, inform systemd that wlproxy is ready using
    // notify. Other services that depend on wlproxy can start now.
    if let Ok(true) = sd_notify::booted() {
        if args.debug {
            eprintln!("Init detected as being systemd. Notifying of readiness.");
        }
        if let Err(e) = sd_notify::notify(&[NotifyState::Ready]) {
            eprintln!("Warning, failed to notify systemd with error: {}", e);
        }
    }

    // Listen for connections
    loop {
        if stop() {
            break Ok(());
        }
        let (conn, _) = ctx
            .listener
            .accept()
            .context("Error accepting downstream connection")?;
        let upstream_path = args.upstream.clone().unwrap_or_else(default_upstream);
        let upstream =
            UnixStream::connect(&upstream_path).context("Error creating upstream connection")?;

        handle_connection(conn, upstream, &args);
    }
}

pub fn run_with_stop(args: Args, stop: &AtomicBool) -> Result<(), String> {
    validate_interfaces(&args.block, args.quiet);

    let ctx = setup_listener(&args)?;
    let _defer = defer::defer(|| {
        _ = remove_file(&ctx.lock_path);
    });
    let _defer1 = defer::defer(|| {
        _ = remove_file(&ctx.downstream);
    });

    if let Ok(true) = sd_notify::booted() {
        if args.debug {
            eprintln!("Init detected as being systemd. Notifying of readiness.");
        }
        if let Err(e) = sd_notify::notify(&[NotifyState::Ready]) {
            eprintln!("Warning, failed to notify systemd with error: {}", e);
        }
    }

    // A background thread checks the stop flag and connects to our own
    // downstream socket to unblock accept().
    let waker_path = ctx.downstream.clone();
    std::thread::scope(|s| {
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = UnixStream::connect(&waker_path);
        });

        loop {
            match ctx.listener.accept() {
                Ok((conn, _)) => {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let upstream_path = args.upstream.clone().unwrap_or_else(default_upstream);
                    let upstream = UnixStream::connect(&upstream_path)
                        .context("Error creating upstream connection")?;
                    handle_connection(conn, upstream, &args);
                }
                Err(e) => return Err(format!("Error accepting: {}", e)),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::Shutdown;
    use std::time::Duration;

    fn pair_timeout() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        (a, b)
    }

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    fn send_pkt(mut s: &UnixStream, pkt: &proto::Packet) {
        let mut buf = vec![];
        proto::write_packet(&mut buf, pkt).unwrap();
        s.write_all(&buf).unwrap();
    }

    fn recv_pkt(s: &mut impl std::io::Read) -> proto::Packet {
        proto::read_packet(s).unwrap().unwrap()
    }

    fn base_args() -> Args {
        Args {
            upstream: None,
            app_id: None,
            prefix_app_id: false,
            title: None,
            prefix_title: false,
            block: vec![],
            quiet: true,
            debug: false,
            downstream: PathBuf::from("/nonexistent/test"),
        }
    }

    // ---------------------------------------------------------------------------
    // is_blocked_interface
    // ---------------------------------------------------------------------------

    #[test]
    fn blocked_iface_matches() {
        assert!(is_blocked_interface(
            "wl_surface",
            &["wl_surface".to_string()]
        ));
    }

    #[test]
    fn blocked_iface_no_match() {
        assert!(!is_blocked_interface("wl_surface", &[]));
    }

    // ---------------------------------------------------------------------------
    // Errorize
    // ---------------------------------------------------------------------------

    #[test]
    fn errorize_ok() {
        let r: Result<i32, &str> = Ok(42);
        assert_eq!(r.context("x").unwrap(), 42);
    }

    #[test]
    fn errorize_err() {
        let r: Result<i32, &str> = Err("boom");
        assert_eq!(r.context("x"), Err("x: boom".to_string()));
    }

    // ---------------------------------------------------------------------------
    // known_protocols
    // ---------------------------------------------------------------------------

    #[test]
    fn known_protos_contain_wm_base() {
        let p = known_protocols();
        assert!(p.contains(&"xdg_wm_base"));
    }

    // ---------------------------------------------------------------------------
    // handle_client_to_server
    // ---------------------------------------------------------------------------

    #[test]
    fn c2s_passthrough() {
        let (client, downstream) = pair_timeout();
        let (upstream, mut server) = pair();

        let objects = Arc::new(Mutex::new(HashMap::from([(1u32, ObjType::Display)])));
        let xdgwmbase_type_id = Arc::new(Mutex::new(None));
        let blocked_objects = Arc::new(Mutex::new(HashSet::new()));
        let args = base_args();

        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );

        client.shutdown(Shutdown::Write).unwrap();
        drop(client);

        handle_client_to_server(
            &downstream,
            &upstream,
            &objects,
            &xdgwmbase_type_id,
            &blocked_objects,
            &args,
        )
        .unwrap();

        let pkt = recv_pkt(&mut server);
        assert_eq!(pkt.id, 1);
        assert_eq!(pkt.opcode, 1);
        let new_id = proto::read_arg_uint(&mut Cursor::new(&pkt.body)).unwrap();
        assert_eq!(new_id, 2);
        assert_eq!(objects.lock().unwrap().get(&2), Some(&ObjType::Registry));
    }

    #[test]
    fn c2s_app_id_replacement() {
        let (client, downstream) = pair_timeout();
        let (upstream, mut server) = pair();

        let objects = Arc::new(Mutex::new(HashMap::from([(1u32, ObjType::Display)])));
        let xdgwmbase_type_id = Arc::new(Mutex::new(Some((0, 1))));
        let blocked_objects = Arc::new(Mutex::new(HashSet::new()));
        let args = Args {
            app_id: Some("my-app".to_string()),
            ..base_args()
        };

        // Display -> get_registry -> id=2
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );

        // Registry -> bind -> interface "xdg_wm_base" with type_id=0 -> id=3
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );

        // XdgWmBase -> get_xdg_surface -> id=4
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 3,
                opcode: 2,
                body,
            },
        );

        // XdgSurface -> get_toplevel -> id=5
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 4,
                opcode: 1,
                body,
            },
        );

        // XdgToplevel -> set_app_id
        let mut body = vec![];
        proto::write_arg_string(&mut body, "orig").unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 5,
                opcode: 3,
                body,
            },
        );

        client.shutdown(Shutdown::Write).unwrap();
        drop(client);

        handle_client_to_server(
            &downstream,
            &upstream,
            &objects,
            &xdgwmbase_type_id,
            &blocked_objects,
            &args,
        )
        .unwrap();

        // Drain the 4 non-modified packets
        for _ in 0..4 {
            recv_pkt(&mut server);
        }

        let pkt = recv_pkt(&mut server);
        assert_eq!(pkt.id, 5);
        assert_eq!(pkt.opcode, 3);
        let app_id = proto::read_arg_string(&mut Cursor::new(&pkt.body)).unwrap();
        assert_eq!(app_id, Some("my-app".to_string()));
    }

    #[test]
    fn c2s_title_replacement() {
        let (client, downstream) = pair_timeout();
        let (upstream, mut server) = pair();

        let objects = Arc::new(Mutex::new(HashMap::from([(1u32, ObjType::Display)])));
        let xdgwmbase_type_id = Arc::new(Mutex::new(Some((0, 1))));
        let blocked_objects = Arc::new(Mutex::new(HashSet::new()));
        let args = Args {
            title: Some("my-title".to_string()),
            ..base_args()
        };

        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );

        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );

        let mut body = vec![];
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 3,
                opcode: 2,
                body,
            },
        );

        let mut body = vec![];
        proto::write_arg_uint(&mut body, 5).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 4,
                opcode: 1,
                body,
            },
        );

        let mut body = vec![];
        proto::write_arg_string(&mut body, "orig-title").unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 5,
                opcode: 2,
                body,
            },
        );

        client.shutdown(Shutdown::Write).unwrap();
        drop(client);

        handle_client_to_server(
            &downstream,
            &upstream,
            &objects,
            &xdgwmbase_type_id,
            &blocked_objects,
            &args,
        )
        .unwrap();

        for _ in 0..4 {
            recv_pkt(&mut server);
        }

        let pkt = recv_pkt(&mut server);
        assert_eq!(pkt.id, 5);
        assert_eq!(pkt.opcode, 2);
        let title = proto::read_arg_string(&mut Cursor::new(&pkt.body)).unwrap();
        assert_eq!(title, Some("my-title".to_string()));
    }

    #[test]
    fn c2s_app_id_prefix() {
        let (client, downstream) = pair_timeout();
        let (upstream, mut server) = pair();

        let objects = Arc::new(Mutex::new(HashMap::from([(1u32, ObjType::Display)])));
        let xdgwmbase_type_id = Arc::new(Mutex::new(Some((0, 1))));
        let blocked_objects = Arc::new(Mutex::new(HashSet::new()));
        let args = Args {
            app_id: Some("pfx-".to_string()),
            prefix_app_id: true,
            ..base_args()
        };

        let mut msg_body = vec![];
        proto::write_arg_uint(&mut msg_body, 2).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 1,
                opcode: 1,
                body: msg_body,
            },
        );

        let mut msg_body = vec![];
        proto::write_arg_uint(&mut msg_body, 0).unwrap();
        proto::write_arg_string(&mut msg_body, "xdg_wm_base").unwrap();
        proto::write_arg_uint(&mut msg_body, 1).unwrap();
        proto::write_arg_uint(&mut msg_body, 3).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 2,
                opcode: 0,
                body: msg_body,
            },
        );

        let mut msg_body = vec![];
        proto::write_arg_uint(&mut msg_body, 4).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 3,
                opcode: 2,
                body: msg_body,
            },
        );

        let mut msg_body = vec![];
        proto::write_arg_uint(&mut msg_body, 5).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 4,
                opcode: 1,
                body: msg_body,
            },
        );

        let mut msg_body = vec![];
        proto::write_arg_string(&mut msg_body, "orig").unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 5,
                opcode: 3,
                body: msg_body,
            },
        );

        client.shutdown(Shutdown::Write).unwrap();
        drop(client);

        handle_client_to_server(
            &downstream,
            &upstream,
            &objects,
            &xdgwmbase_type_id,
            &blocked_objects,
            &args,
        )
        .unwrap();

        for _ in 0..4 {
            recv_pkt(&mut server);
        }

        let pkt = recv_pkt(&mut server);
        assert_eq!(pkt.id, 5);
        assert_eq!(pkt.opcode, 3);
        let app_id = proto::read_arg_string(&mut Cursor::new(&pkt.body)).unwrap();
        assert_eq!(app_id, Some("pfx-orig".to_string()));
    }

    #[test]
    fn c2s_blocked_interface_drops_bind() {
        let (client, downstream) = pair();
        let (upstream, mut server) = pair();

        let objects = Arc::new(Mutex::new(HashMap::from([(1u32, ObjType::Display)])));
        let xdgwmbase_type_id = Arc::new(Mutex::new(None));
        let blocked_objects = Arc::new(Mutex::new(HashSet::new()));
        let args = Args {
            block: vec!["wl_output".to_string()],
            ..base_args()
        };

        // get_registry -> id=2
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 2).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 1,
                opcode: 1,
                body,
            },
        );

        // bind wl_output (blocked) -> id=3
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_output").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_uint(&mut body, 3).unwrap();
        send_pkt(
            &client,
            &proto::Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );

        // Message to blocked object id=3 (should be dropped)
        send_pkt(
            &client,
            &proto::Packet {
                id: 3,
                opcode: 0,
                body: vec![],
            },
        );

        let handle = spawn(move || {
            handle_client_to_server(
                &downstream,
                &upstream,
                &objects,
                &xdgwmbase_type_id,
                &blocked_objects,
                &args,
            )
        });

        // Give the handler time to process all messages, then force EOF
        std::thread::sleep(Duration::from_millis(100));
        drop(client);

        // Wait for handler with timeout
        handle.join().unwrap().unwrap();

        // Only get_registry should pass through
        let pkt = recv_pkt(&mut server);
        assert_eq!(pkt.id, 1);
        assert_eq!(pkt.opcode, 1);

        // Server should have nothing else
        assert!(proto::read_packet(&mut server).unwrap().is_none());
    }

    // ---------------------------------------------------------------------------
    // handle_server_to_client
    // ---------------------------------------------------------------------------

    #[test]
    fn s2c_passthrough() {
        let (compositor, upstream) = pair_timeout();
        let (downstream, mut client) = pair();

        let objects = Arc::new(Mutex::new(HashMap::from([(2u32, ObjType::Registry)])));
        let xdgwmbase_type_id = Arc::new(Mutex::new(None));
        let args = base_args();

        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_output").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        send_pkt(
            &compositor,
            &proto::Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );

        compositor.shutdown(Shutdown::Write).unwrap();
        drop(compositor);

        handle_server_to_client(&upstream, &downstream, &objects, &xdgwmbase_type_id, &args)
            .unwrap();

        let pkt = recv_pkt(&mut client);
        assert_eq!(pkt.id, 2);
        assert_eq!(pkt.opcode, 0);
        let mut cursor = Cursor::new(&pkt.body);
        let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
        let name = proto::read_arg_string(&mut cursor).unwrap();
        assert_eq!(name, Some("wl_output".to_string()));
    }

    #[test]
    fn s2c_global_filtering() {
        let (compositor, upstream) = pair_timeout();
        let (downstream, mut client) = pair();

        let objects = Arc::new(Mutex::new(HashMap::from([(2u32, ObjType::Registry)])));
        let xdgwmbase_type_id = Arc::new(Mutex::new(None));
        let args = Args {
            block: vec!["wl_output".to_string()],
            ..base_args()
        };

        // Global event for blocked interface
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 0).unwrap();
        proto::write_arg_string(&mut body, "wl_output").unwrap();
        proto::write_arg_uint(&mut body, 1).unwrap();
        send_pkt(
            &compositor,
            &proto::Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );

        // Global event for non-blocked interface
        let mut body = vec![];
        proto::write_arg_uint(&mut body, 1).unwrap();
        proto::write_arg_string(&mut body, "wl_compositor").unwrap();
        proto::write_arg_uint(&mut body, 4).unwrap();
        send_pkt(
            &compositor,
            &proto::Packet {
                id: 2,
                opcode: 0,
                body,
            },
        );

        compositor.shutdown(Shutdown::Write).unwrap();
        drop(compositor);

        handle_server_to_client(&upstream, &downstream, &objects, &xdgwmbase_type_id, &args)
            .unwrap();

        // Only wl_compositor should pass through
        let pkt = recv_pkt(&mut client);
        assert_eq!(pkt.id, 2);
        let mut cursor = Cursor::new(&pkt.body);
        let _type_id = proto::read_arg_uint(&mut cursor).unwrap();
        let name = proto::read_arg_string(&mut cursor).unwrap();
        assert_eq!(name, Some("wl_compositor".to_string()));
    }

    #[test]
    fn validate_interfaces_quiet_suppresses_warning() {
        // Just verify no panic when called with quiet=true.
        validate_interfaces(&["UnknownInterface".to_string()], true);
    }

    #[test]
    fn known_protos_excludes_arbitrary_string() {
        assert!(!known_protocols().contains(&"UnknownInterface"));
    }
}
