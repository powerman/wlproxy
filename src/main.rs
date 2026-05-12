// Needed on Rust nightlies where this API is still unstable.
#![cfg_attr(need_unix_ancillary_feature, feature(unix_socket_ancillary_data))]

use {
    aargvark::{vark, Aargvark},
    rustix::{
        fd::{AsFd, FromRawFd, OwnedFd, RawFd},
        fs::{flock, OpenOptionsExt},
    },
    sd_notify::NotifyState,
    std::{
        collections::HashMap,
        fmt::Display,
        fs::{remove_file, File},
        io::{Cursor, IoSlice, IoSliceMut},
        os::unix::net::{AncillaryData, SocketAncillary, UnixListener, UnixStream},
        path::PathBuf,
        process::exit,
        sync::{Arc, Mutex},
        thread::spawn,
    },
};

use filterway::proto::{self, read_arg_string};

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
    prefix: Option<()>,
    /// Force all xdg toplevels to have the same title
    #[vark(flag = "--title")]
    title: Option<String>,
    /// Prefix the title instead of replacing
    prefix_title: Option<()>,
    /// Print debug messages
    debug: Option<()>,
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

struct AncillaryReader<'a> {
    reader: &'a UnixStream,
    ancillary_mem: &'a mut [u8],
    fds: &'a mut Vec<RawFd>,
}

impl<'a> std::io::Read for AncillaryReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut ancillary = SocketAncillary::new(self.ancillary_mem);
        let res = self
            .reader
            .recv_vectored_with_ancillary(&mut [IoSliceMut::new(buf)], &mut ancillary);
        if ancillary.truncated() {
            panic!("Ancillary buffer too small");
        }
        for m in ancillary.messages() {
            let Ok(AncillaryData::ScmRights(m)) = m else {
                continue;
            };
            self.fds.extend(m);
        }
        res
    }
}

struct AncillaryWriter<'a, 'b> {
    writer: &'a UnixStream,
    ancillary: SocketAncillary<'b>,
}

impl<'a, 'b> AncillaryWriter<'a, 'b> {
    fn new(writer: &'a UnixStream, ancillary_mem: &'b mut [u8], fds: &Vec<RawFd>) -> Self {
        let mut ancillary = SocketAncillary::new(ancillary_mem);
        ancillary.add_fds(fds.as_ref());
        Self { writer, ancillary }
    }
}

impl<'a, 'b> std::io::Write for AncillaryWriter<'a, 'b> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let res = self
            .writer
            .send_vectored_with_ancillary(&[IoSlice::new(buf)], &mut self.ancillary);
        self.ancillary.clear();
        res
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

fn main() {
    fn inner() -> Result<(), String> {
        let args = vark::<Args>();
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

        // If the system booted with systemd, inform systemd that filterway is ready using
        // notify. Other services that depend on filterway can start now.
        if let Ok(true) = sd_notify::booted() {
            if args.debug.is_some() {
                eprintln!("Init detected as being systemd. Notifying of readiness.");
            }
            if let Err(e) = sd_notify::notify(true, &[NotifyState::Ready]) {
                eprintln!("Warning, failed to notify systemd with error: {}", e);
            }
        }

        // Listen for connections
        loop {
            let (downstream, _) = downstream
                .accept()
                .context("Error accepting downstream connection")?;
            let upstream = UnixStream::connect(&args.upstream)
                .context("Error creating upstream connection")?;

            #[derive(Clone, Copy, Debug)]
            enum ObjType {
                Display,
                Registry,
                XdgWmBase { ver: u32 },
                XdgSurface { ver: u32 },
                XdgToplevel { ver: u32 },
            }

            let objects = Arc::new(Mutex::new(HashMap::new()));
            objects.lock().unwrap().insert(1, ObjType::Display);
            let xdgwmbase_type_id = Arc::new(Mutex::new(None));
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
                    match (|| -> Result<(), String> {
                        let mut ancillary_mem = [0u8; 128];
                        let mut ancillary_accum = vec![];
                        // Wait for next message
                        while let Some(mut packet) = proto::read_packet(&mut AncillaryReader {
                            reader: &downstream,
                            ancillary_mem: &mut ancillary_mem,
                            fds: &mut ancillary_accum,
                        })
                        .context("Error reading message")?
                        {
                            // Track and prepare manipulations
                            {
                                let mut objects = objects.lock().unwrap();
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
                                            // Get registry
                                            if packet.opcode == 1 {
                                                let mut cursor = Cursor::new(&packet.body);
                                                let obj_id = proto::read_arg_uint(&mut cursor)
                                                    .context("Error reading registry id")?;
                                                objects.insert(obj_id, ObjType::Registry);
                                            }
                                        }
                                        ObjType::Registry => {
                                            // Bind
                                            if packet.opcode == 0 {
                                                let mut cursor = Cursor::new(&packet.body);
                                                let obj_type_id = proto::read_arg_uint(&mut cursor)
                                                    .context(
                                                        "Error/eof reading bind object type id",
                                                    )?;

                                                // Arbitrary snowflake magic param - interface name
                                                proto::read_arg_string(&mut cursor).context(
                                                    "Error reading bind message type string",
                                                )?;

                                                // Arbitrary snowflake magic param - version
                                                let version = proto::read_arg_uint(&mut cursor)
                                                    .context(
                                                        "Error reading bind message version",
                                                    )?;
                                                let obj_id = proto::read_arg_uint(&mut cursor)
                                                    .context("Error/eof reading bind object id")?;
                                                if let Some((want_type_id, _version)) =
                                                    *xdgwmbase_type_id.lock().unwrap()
                                                {
                                                    if obj_type_id == want_type_id {
                                                        objects.insert(
                                                            obj_id,
                                                            ObjType::XdgWmBase {
                                                                // prefer the magic param version because it's nearer to the use location...
                                                                ver: version,
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        ObjType::XdgWmBase { ver } => {
                                            match ver {
                                                0 ..= 6 => {
                                                    // Get surface
                                                    if packet.opcode == 2 {
                                                        let mut cursor = Cursor::new(&packet.body);
                                                        let obj_id =
                                                            proto::read_arg_uint(
                                                                &mut cursor,
                                                            ).context("Error reading xdg wm base create surface id")?;
                                                        objects.insert(obj_id, ObjType::XdgSurface { ver });
                                                    }
                                                },
                                                _ => panic!(
                                                    "Unsupported xdg_wm_base object version {}_{}_{}_{}",
                                                    ver,
                                                    ver,
                                                    ver,
                                                    ver
                                                ),
                                            }
                                        }
                                        ObjType::XdgSurface { ver } => {
                                            match ver {
                                                0..=6 => {
                                                    // Create toplevel
                                                    if packet.opcode == 1 {
                                                        let mut cursor = Cursor::new(&packet.body);
                                                        let obj_id =
                                                            proto::read_arg_uint(
                                                                &mut cursor,
                                                            ).context(
                                                                "Error reading xdg surface create toplevel id",
                                                            )?;
                                                        objects.insert(
                                                            obj_id,
                                                            ObjType::XdgToplevel { ver },
                                                        );
                                                    }
                                                }
                                                _ => panic!(
                                                    "Unsupported xdg_surface object version {}",
                                                    ver
                                                ),
                                            }
                                        }
                                        ObjType::XdgToplevel { ver } => {
                                            match ver {
                                                0..=6 => {
                                                    match packet.opcode {
                                                        // set_title
                                                        2 => {
                                                            if let Some(title) = &args.title {
                                                                let read_title =
                                                                read_arg_string(
                                                                    &mut packet.body.as_slice(),
                                                                ).context("Error reading app id message body")?;
                                                                packet.body.clear();
                                                                let new_title =
                                                                    if args.prefix_title.is_some() {
                                                                        format!(
                                                                            "{}{}",
                                                                            title,
                                                                            read_title
                                                                                .unwrap_or_default(
                                                                                )
                                                                        )
                                                                    } else {
                                                                        title.clone()
                                                                    };
                                                                proto::write_arg_string(
                                                                    &mut packet.body,
                                                                    &new_title,
                                                                )
                                                                .unwrap();
                                                                if args.debug.is_some() {
                                                                    eprintln!(
                                                                    "Modified title; new message: {:?}",
                                                                    packet
                                                                );
                                                                }
                                                            }
                                                        }
                                                        // set_app_id
                                                        3 => {
                                                            if let Some(app_id) = &args.app_id {
                                                                let read_app_id =
                                                                read_arg_string(
                                                                    &mut packet.body.as_slice(),
                                                                ).context("Error reading app id message body")?;
                                                                packet.body.clear();
                                                                let new_app_id =
                                                                    if args.prefix.is_some() {
                                                                        format!(
                                                                            "{}{}",
                                                                            app_id,
                                                                            read_app_id
                                                                                .unwrap_or_default(
                                                                                )
                                                                        )
                                                                    } else {
                                                                        app_id.clone()
                                                                    };
                                                                proto::write_arg_string(
                                                                    &mut packet.body,
                                                                    &new_app_id,
                                                                )
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
                                                    }
                                                }
                                                _ => panic!(
                                                    "Unsupported xdg_toplevel object version {}",
                                                    ver
                                                ),
                                            }
                                        }
                                    }
                                }
                            }

                            // Forward message with retractions/additions
                            proto::write_packet(
                                &mut AncillaryWriter::new(
                                    &upstream,
                                    &mut ancillary_mem,
                                    &ancillary_accum,
                                ),
                                &packet,
                            )
                            .context("Error writing message")?;
                            for fd in ancillary_accum.drain(..) {
                                // Safety: FDs were received via SCM_RIGHTS ancillary data,
                                // which transfers ownership to the receiver.  Re-wrapping
                                // in OwnedFd ensures they are closed on drop.
                                drop(unsafe { OwnedFd::from_raw_fd(fd) });
                            }
                        }
                        Ok(())
                    })() {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("Warning, client->server thread exiting with error: {}", e);
                        }
                    }
                }
            });
            spawn({
                let downstream = downstream.try_clone().unwrap();
                let mut upstream = upstream.try_clone().unwrap();
                let objects = objects.clone();
                move || {
                    let _defer = defer::defer({
                        let downstream = downstream.try_clone().unwrap();
                        let upstream = upstream.try_clone().unwrap();
                        move || {
                            _ = downstream.shutdown(std::net::Shutdown::Both);
                            _ = upstream.shutdown(std::net::Shutdown::Both);
                        }
                    });
                    match (|| -> Result<(), String> {
                        let mut ancillary_mem = [0u8; 128];
                        let mut ancillary_accum = vec![];
                        let mut cache_reg_id = None;
                        // Read next packet
                        while let Some(packet) = proto::read_packet(&mut AncillaryReader {
                            reader: &mut upstream,
                            ancillary_mem: &mut ancillary_mem,
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

                            // Tracking and manipulation
                            // Ack delete, hardcoded display
                            if (packet.id, packet.opcode) == (1, 1) {
                                let mut cursor = Cursor::new(&packet.body);
                                let obj_id = proto::read_arg_uint(&mut cursor)
                                    .context("Error reading display delete obj id")?;
                                objects.lock().unwrap().remove(&obj_id);
                            }
                            if let Some(reg_id) = match &cache_reg_id {
                                Some(r) => Some(*r),
                                None => {
                                    if let Some(ObjType::Registry) =
                                        objects.lock().unwrap().get(&packet.id)
                                    {
                                        cache_reg_id = Some(packet.id);
                                        Some(packet.id)
                                    } else {
                                        None
                                    }
                                }
                            } {
                                if reg_id == packet.id {
                                    // global
                                    if packet.opcode == 0 {
                                        let mut cursor = Cursor::new(&packet.body);
                                        let type_id = proto::read_arg_uint(&mut cursor)
                                            .context("Error reading global message type id")?;
                                        let type_str = proto::read_arg_string(&mut cursor)
                                            .context("Error reading global message type string")?;
                                        let version = proto::read_arg_uint(&mut cursor)
                                            .context("Error reading global message version")?;
                                        if type_str.as_deref() == Some("xdg_wm_base") {
                                            *xdgwmbase_type_id.lock().unwrap() =
                                                Some((type_id, version));
                                        }
                                    }
                                }
                            }

                            // Forward messages
                            proto::write_packet(
                                &mut AncillaryWriter::new(
                                    &downstream,
                                    &mut ancillary_mem,
                                    &ancillary_accum,
                                ),
                                &packet,
                            )
                            .context("Error writing message")?;
                            for fd in ancillary_accum.drain(..) {
                                // Safety: FDs were received via SCM_RIGHTS ancillary data,
                                // which transfers ownership to the receiver.  Re-wrapping
                                // in OwnedFd ensures they are closed on drop.
                                drop(unsafe { OwnedFd::from_raw_fd(fd) });
                            }
                        }
                        Ok(())
                    })() {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("Warning, server->client thread exiting with error: {}", e);
                        }
                    }
                }
            });
        }
    }

    match inner() {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{}", e);
            exit(1);
        }
    }
}
