use {
    aargvark::{vark_explicit, Aargvark, VarkRet},
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

use uds::UnixStreamExt;
use wlproxy::proto::{self, read_arg_string};
use wlproxy::ObjType;

#[derive(Aargvark, Clone)]
struct Args {
    /// Full path to primary compositor Wayland socket (like `/run/user/1000/wayland-0`)
    #[vark(flag = "--upstream")]
    upstream: PathBuf,
    /// Full path for new Wayland socket
    #[vark(flag = "--downstream")]
    downstream: PathBuf,
    /// Force all xdg toplevels to have the same app id
    #[vark(flag = "--app-id")]
    app_id: Option<String>,
    /// Prefix the app id instead of replacing
    #[vark(flag = "--prefix-app-id")]
    prefix_app_id: Option<()>,
    /// Force all xdg toplevels to have the same title
    #[vark(flag = "--title")]
    title: Option<String>,
    /// Prefix the title instead of replacing
    prefix_title: Option<()>,
    /// Wayland interfaces to block (can be specified multiple times)
    #[vark(flag = "--block")]
    block: Option<String>,
    /// Suppress warnings about unknown interface names
    #[vark(flag = "--quiet")]
    quiet: Option<()>,
    /// Print debug messages
    debug: Option<()>,
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

fn preprocess_args() -> Vec<String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 {
        return args;
    }

    let mut block_values = Vec::new();
    let mut remaining = Vec::new();
    let mut i = 1;

    while i < args.len() {
        if args[i] == "--block" {
            i += 1;
            if i < args.len() && !args[i].starts_with("--") {
                block_values.push(args[i].clone());
            }
        } else {
            remaining.push(args[i].clone());
        }
        i += 1;
    }

    let mut result = vec![args[0].clone()];
    result.extend(remaining);
    if !block_values.is_empty() {
        result.push("--block".to_string());
        result.push(block_values.join(","));
    }
    result
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

fn is_blocked_interface(name: &str, block: &Option<String>) -> bool {
    block
        .as_deref()
        .is_some_and(|list| list.split(',').any(|b| b == name))
}

fn validate_interfaces(block: &Option<String>, quiet: bool) {
    let Some(list) = block.as_deref() else {
        return;
    };
    if quiet {
        return;
    }
    for name in list.split(',') {
        if !known_protocols().contains(&name) {
            eprintln!(
                "Warning: unknown Wayland interface \"{}\" in --block list",
                name
            );
        }
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
    _xdgwmbase_type_id: &Arc<Mutex<Option<(u32, u32)>>>,
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
                if args.debug.is_some() {
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
                                        if args.debug.is_some() {
                                            eprintln!("Blocked bind for interface: {}", name);
                                        }
                                        blocked.insert(obj_id);
                                        true
                                    } else if let Some((want_type_id, _version)) =
                                        *_xdgwmbase_type_id.lock().unwrap()
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
                                            let new_title = if args.prefix_title.is_some() {
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
                                            if args.debug.is_some() {
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
                                            let new_app_id = if args.prefix_app_id.is_some() {
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
                                            if args.debug.is_some() {
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
            for fd in ancillary_accum.drain(..) {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            continue;
        }

        proto::write_packet(
            &mut AncillaryWriter::new(upstream, &ancillary_accum),
            &packet,
        )
        .context("Error writing message")?;
        for fd in ancillary_accum.drain(..) {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
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
        if args.debug.is_some() {
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
                    if args
                        .block
                        .as_deref()
                        .is_some_and(|list| list.split(',').any(|b| b == name))
                    {
                        if args.debug.is_some() {
                            eprintln!("Blocked global: {}", name);
                        }
                        for fd in ancillary_accum.drain(..) {
                            drop(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
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
        for fd in ancillary_accum.drain(..) {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let processed_args = preprocess_args();
    let args = match vark_explicit::<Args>(
        Some(processed_args[0].clone()),
        processed_args[1..].to_vec(),
    ) {
        Ok(VarkRet::Ok(a)) => a,
        Ok(VarkRet::Help(h)) => {
            println!("{}", h.render());
            return Ok(());
        }
        Err(e) => {
            eprintln!("{:?}", e);
            std::process::exit(1);
        }
    };

    validate_interfaces(&args.block, args.quiet.is_some());

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
        ).context("Error getting exclusive lock for downstream listener, is another compositor already listening?")?;
    let _defer = defer::defer(|| {
        _ = remove_file(&lock_path);
    });
    _ = remove_file(&args.downstream);
    let downstream =
        UnixListener::bind(&args.downstream).context("Error creating downstream listener")?;
    let _defer1 = defer::defer(|| {
        _ = remove_file(&args.downstream);
    });

    // If the system booted with systemd, inform systemd that wlproxy is ready using
    // notify. Other services that depend on wlproxy can start now.
    if let Ok(true) = sd_notify::booted() {
        if args.debug.is_some() {
            eprintln!("Init detected as being systemd. Notifying of readiness.");
        }
        if let Err(e) = sd_notify::notify(&[NotifyState::Ready]) {
            eprintln!("Warning, failed to notify systemd with error: {}", e);
        }
    }

    // Listen for connections
    loop {
        let (downstream, _) = downstream
            .accept()
            .context("Error accepting downstream connection")?;
        let upstream =
            UnixStream::connect(&args.upstream).context("Error creating upstream connection")?;

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
                if let Err(e) = handle_server_to_client(
                    &upstream,
                    &downstream,
                    &objects,
                    &xdgwmbase_type_id,
                    &args,
                ) {
                    eprintln!("Warning, server->client thread exiting with error: {}", e);
                }
            }
        });
    }
}
