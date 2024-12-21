/* SPDX-License-Identifier: GPL-3.0-or-later */
use clap::{value_parser, Arg, ArgAction, Command};
use log::{debug, error, Log, Record};
use nix::errno::Errno;
use nix::libc;
use nix::poll::{PollFd, PollFlags};
use nix::sys::{signal, socket, wait};
use nix::{fcntl, unistd};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

mod bench;
mod compress;
mod damage;
mod dmabuf;
mod kernel;
mod mainloop;
mod mirror;
mod read;
mod secctx;
mod stub;
mod tracking;
mod util;
mod video;
mod wayland;
mod wayland_gen;

use crate::mainloop::*;
use crate::util::*;

/** Logger configuration data */
struct Logger {
    max_level: log::LevelFilter,
    pid: u32,
    color_output: bool,
    anti_staircase: bool,
    color: usize,
    label: &'static str,
}

impl Log for Logger {
    fn enabled(&self, meta: &log::Metadata<'_>) -> bool {
        meta.level() <= self.max_level
    }
    fn log(&self, record: &Record<'_>) {
        if record.level() > self.max_level {
            return;
        }

        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        let t = if let Ok(t) = time {
            (t.as_nanos() % 100000000000u128) / 1000u128
        } else {
            0
        };
        let (esc1a, esc1b, esc1c) = if self.color_output {
            let c = if self.color == 0 {
                "36"
            } else if self.color == 1 {
                "34"
            } else {
                "35"
            };
            if record.level() <= log::Level::Error {
                ("\x1b[0;", c, ";1m")
            } else {
                ("\x1b[0;", c, "m")
            }
        } else {
            ("", "", "")
        };
        let esc2 = if self.color_output { "\x1b[0m" } else { "" };
        let esc3 = if self.anti_staircase { "\r\n" } else { "\n" };
        let lvl_str: &str = match record.level() {
            log::Level::Error => "ERR",
            log::Level::Warn => "Wrn",
            log::Level::Debug => "dbg",
            log::Level::Info => "inf",
            log::Level::Trace => "trc",
        };

        const MAX_LOG_LEN: usize = 512;
        let mut buf = [0u8; MAX_LOG_LEN];
        let mut cursor = std::io::Cursor::new(&mut buf[..MAX_LOG_LEN - 5]);
        let _ = write!(
            &mut cursor,
            "{}{}{}[{:02}.{:06} {} {}({}) {}:{}]{} {}{}",
            esc1a,
            esc1b,
            esc1c,
            t / 1000000u128,
            t % 1000000u128,
            lvl_str,
            self.label,
            self.pid,
            record
                .file()
                .unwrap_or("src/unknown")
                .strip_prefix("src/")
                .unwrap(),
            record.line().unwrap_or(0),
            esc2,
            record.args(),
            esc3
        );
        let mut str_end = cursor.position() as usize;
        if str_end >= MAX_LOG_LEN - 9 {
            /* Deal with possible partial UTF-8 char */
            str_end = match std::str::from_utf8(&buf[..str_end]) {
                Ok(x) => x.len(),
                Err(y) => y.valid_up_to(),
            };
        }
        if str_end >= MAX_LOG_LEN - 9 {
            /* Assume message was truncated */
            assert!(str_end <= MAX_LOG_LEN - 5, "{} {}", str_end, MAX_LOG_LEN);
            buf[str_end..str_end + 3].fill(b'.');
            if self.anti_staircase {
                buf[str_end + 3] = b'\r';
                buf[str_end + 4] = b'\n';
                str_end += 5;
            } else {
                buf[str_end + 3] = b'\n';
                str_end += 4;
            }
        }
        let handle = &mut std::io::stderr().lock();
        let _ = handle.write_all(&buf[..str_end]);
        let _ = handle.flush();
    }
    fn flush(&self) {
        /* not needed */
    }
}

/** Return a random list of 10 alphanumeric characters */
fn get_rand_tag() -> Result<[u8; 10], String> {
    let mut rand_buf = [0_u8; 16];
    getrandom::getrandom(&mut rand_buf).map_err(|x| tag!("Failed to get random bits: {}", x))?;
    let mut n: u128 = u128::from_le_bytes(rand_buf);

    // Note: log2(62^10) ≈ 59.5 which is ≪ 128, so the resulting
    // random strings are only very slightly biased.
    let mut rand_tag = [0u8; 10];
    for i in rand_tag.iter_mut() {
        let v = (n % 62) as u32;
        n /= 62;
        *i = if v < 26 {
            (v + ('a' as u32)) as u8
        } else if v < 52 {
            (v - 26 + ('A' as u32)) as u8
        } else {
            (v - 52 + ('0' as u32)) as u8
        }
    }
    Ok(rand_tag)
}

/** Flags for `open()` to open a reference to a directory */
#[cfg(target_os = "linux")]
fn dir_flags() -> fcntl::OFlag {
    /* O_PATH is from 2.6.39 Linux */
    nix::fcntl::OFlag::O_PATH | nix::fcntl::OFlag::O_DIRECTORY
}
#[cfg(not(target_os = "linux"))]
fn dir_flags() -> fcntl::OFlag {
    nix::fcntl::OFlag::O_DIRECTORY
}

/** Get a file descriptor corresponding to a path, suitable for `fchdir()` */
fn open_folder(p: &Path) -> Result<OwnedFd, String> {
    let raw_fd = nix::fcntl::open(
        p,
        dir_flags() | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|x| tag!("Failed to open folder '{:?}': {}", p, x))?;
    Ok(unsafe {
        // SAFETY: freshly created, checked valid, exclusively owned
        OwnedFd::from_raw_fd(raw_fd)
    })
}

/** Connection information for a VSOCK socket */
#[derive(Debug, Copy, Clone)]
struct VSockConfig {
    // todo: use option for (to_host, cid)?
    to_host: bool,
    cid: u32,
    port: u32,
}

/** Specification for a Unix (or VSOCK) socket that Waypipe may bind or connect to. */
#[derive(Debug, Clone)]
enum SocketSpec {
    VSock(VSockConfig),
    Unix(PathBuf),
}

/* Wrapper for `libc::VMADDR_CID_HOST` or 0 if not supported */
#[cfg(target_os = "linux")]
const VMADDR_CID_HOST: u32 = libc::VMADDR_CID_HOST;
#[cfg(not(target_os = "linux"))]
const VMADDR_CID_HOST: u32 = 0;

impl FromStr for VSockConfig {
    type Err = &'static str;

    fn from_str(mut s: &str) -> Result<Self, Self::Err> {
        const FAILURE: &str = "VSOCK spec should have format [[s]CID:]port";

        let (to_host, cid) = if let Some((mut prefix, suffix)) = s.split_once(':') {
            let to_host = if prefix.starts_with('s') {
                prefix = &prefix[1..];
                true
            } else {
                false
            };
            let cid = prefix.parse::<u32>().map_err(|_| FAILURE)?;
            s = suffix;
            (to_host, cid)
        } else {
            (false, VMADDR_CID_HOST)
        };
        let port = s.parse::<u32>().map_err(|_| FAILURE)?;

        Ok(VSockConfig { to_host, cid, port })
    }
}

impl fmt::Display for VSockConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.cid != VMADDR_CID_HOST {
            write!(f, "{}", self.port)
        } else {
            let prefix = if self.to_host { "s" } else { "" };
            write!(f, "{}{}:{}", prefix, self.cid, self.port)
        }
    }
}

/** Remove the dead child indicated by `pid` from the connection set */
fn prune_connections(connections: &mut BTreeMap<u32, std::process::Child>, pid: nix::unistd::Pid) {
    if let Some(mut child) = connections.remove(&(pid.as_raw() as u32)) {
        debug!("Waiting for dead child {} to reveal status", child.id());
        let _ = child.wait();
        debug!("Status received");
    } else {
        error!("Received SIGCHLD for unexpected child: {}", pid.as_raw());
    }
}

/** Wait until all processes in the set have died */
fn wait_for_connnections(mut connections: BTreeMap<u32, std::process::Child>) {
    while let Some((_, mut child)) = connections.pop_first() {
        debug!("Waiting for dead child {} to reveal status", child.id());
        let _ = child.wait();
        debug!("Status received");
    }
}

/** Create the argument list for `waypipe client-conn` or `waypipe server-conn`
 *
 * New strings allocated for this will be stored into `strings`.
 */
fn build_connection_command<'a>(
    strings: &'a mut Vec<OsString>,
    socket_path: &'a SocketSpec,
    options: &'a Options,
    client: bool,
    anti_staircase: bool,
) -> Vec<&'a OsStr> {
    let mut args: Vec<&'a OsStr> = Vec::new();

    strings.push(OsString::from(options.compression.to_string()));
    strings.push(OsString::from(options.threads.to_string()));
    strings.push(OsString::from(format!("--video={}", options.video)));
    match socket_path {
        SocketSpec::VSock(x) => strings.push(format!("{}", x).into()),
        SocketSpec::Unix(x) => strings.push(x.into()),
    };
    let comp_str = &strings[0];
    let thread_str = &strings[1];
    let vid_str = &strings[2];
    let socket_str = &strings[3];

    /* Unlike the parent process, use short option names for these, making the command
     * line shorter and easier to skim */
    args.push(OsStr::new("-s"));
    args.push(socket_str);
    if matches!(socket_path, SocketSpec::VSock(_)) {
        args.push(OsStr::new("--vsock"));
    }
    if options.debug {
        args.push(OsStr::new("-d"));
    }
    if options.no_gpu {
        args.push(OsStr::new("-n"));
    }
    args.push(OsStr::new("--threads"));
    args.push(thread_str);
    args.push(OsStr::new("-c"));
    args.push(comp_str);
    if !options.title_prefix.is_empty() {
        args.push(OsStr::new("--title-prefix"));
        args.push(OsStr::new(&options.title_prefix));
    }
    if options.video.format.is_some() {
        args.push(vid_str);
    }
    if let Some(d) = &options.drm_node {
        assert!(!client);
        args.push(OsStr::new("--drm-node"));
        args.push(OsStr::new(d));
    }
    if anti_staircase {
        args.push(OsStr::new("--anti-staircase"));
    }
    if client {
        args.push(OsStr::new("client-conn"));
    } else {
        args.push(OsStr::new("server-conn"));
    }
    args
}

/** Send connection header and run the main proxy loop for a new
 * `waypipe server` connection
 */
fn handle_server_conn(
    link_fd: OwnedFd,
    wayland_fd: OwnedFd,
    opts: &Options,
    wire_version_override: Option<u32>,
) -> Result<(), String> {
    /* Note: the last 12 bytes of the header will stay at zero. They would
     * only need be set to a unique (random) value if reconnection support were
     * implemented. */
    let mut header: [u8; 16] = [0_u8; 16];

    let ver = wire_version_override
        .map(|x| x.clamp(MIN_PROTOCOL_VERSION, WAYPIPE_PROTOCOL_VERSION))
        .unwrap_or(WAYPIPE_PROTOCOL_VERSION);

    let ver_hi = ver >> 4;
    let ver_lo = ver & ((1 << 4) - 1);

    let mut lead: u32 = (ver_hi << 16) | (ver_lo << 3) | CONN_FIXED_BIT;
    match opts.compression {
        Compression::None => lead |= CONN_NO_COMPRESSION,
        Compression::Lz4(_) => lead |= CONN_LZ4_COMPRESSION,
        Compression::Zstd(_) => lead |= CONN_ZSTD_COMPRESSION,
    }
    if opts.no_gpu {
        lead |= CONN_NO_DMABUF_SUPPORT;
    }
    if let Some(ref f) = opts.video.format {
        match f {
            VideoFormat::H264 => {
                lead |= CONN_H264_VIDEO;
            }
            VideoFormat::VP9 => {
                lead |= CONN_VP9_VIDEO;
            }
            VideoFormat::AV1 => {
                lead |= CONN_AV1_VIDEO;
            }
        }
    } else {
        lead |= CONN_NO_VIDEO;
    }
    debug!("header: {:0x}", lead);
    header[..4].copy_from_slice(&u32::to_le_bytes(lead));

    nix::unistd::write(&link_fd, &header)
        .map_err(|x| tag!("Failed to write connection header: {}", x))?;

    debug!("have written initial bytes");

    set_nonblock(&link_fd)?;
    set_nonblock(&wayland_fd)?;

    let (sigmask, sigint_received) = setup_sigint_handler()?;
    mainloop::main_interface_loop(
        link_fd,
        wayland_fd,
        opts,
        MIN_PROTOCOL_VERSION,
        false,
        sigmask,
        sigint_received,
    )
}

/** Connect to a socket (and possibly unlink its path); the socket returned is
 * nonblocking and cloexec. */
fn socket_connect(
    spec: &SocketSpec,
    cwd: &OwnedFd,
    unlink_after: bool, /* Unlink after connecting? */
) -> Result<OwnedFd, String> {
    let socket = match spec {
        SocketSpec::Unix(path) => {
            let socket = socket::socket(
                socket::AddressFamily::Unix,
                socket::SockType::Stream,
                socket::SockFlag::SOCK_CLOEXEC,
                None,
            )
            .map_err(|x| tag!("Failed to create socket: {}", x))?;

            let file = path
                .file_name()
                .ok_or_else(|| tag!("Socket path {:?} missing file name", path))?;
            let addr = socket::UnixAddr::new(file)
                .map_err(|x| tag!("Failed to create Unix socket address from file name: {}", x))?;

            let r = if let Some(folder) = path.parent() {
                nix::unistd::chdir(folder).map_err(|x| tag!("Failed to visit folder: {}", x))?;
                // eventually: is a 'connectat' equivalent available?
                // can use /proc/self/fd to workaround socket path length issues
                let x = socket::connect(socket.as_raw_fd(), &addr);
                if x.is_ok() && unlink_after {
                    // race condition possible if socket is moved or replaced, but
                    // unlinking should have no ill effect in such scenarios
                    nix::unistd::unlink(file)
                        .map_err(|x| tag!("Failed to unlink socket: {}", x))?;
                }
                nix::unistd::fchdir(cwd.as_raw_fd())
                    .map_err(|x| tag!("Failed to return to original path: {}", x))?;
                x
            } else {
                let x = socket::connect(socket.as_raw_fd(), &addr);
                if x.is_ok() && unlink_after {
                    nix::unistd::unlink(file)
                        .map_err(|x| tag!("Failed to unlink socket: {}", x))?;
                }
                x
            };
            r.map_err(|x| tag!("Failed to connnect to socket at {:?}: {}", path, x))?;

            socket
        }
        #[cfg(target_os = "linux")]
        SocketSpec::VSock(v) => {
            let socket = socket::socket(
                socket::AddressFamily::Vsock,
                socket::SockType::Stream,
                socket::SockFlag::SOCK_CLOEXEC,
                None,
            )
            .map_err(|x| tag!("Failed to create socket: {}", x))?;

            unsafe {
                /* nix does not yet support svm_flags, so directly use libc */
                const VMADDR_FLAG_TO_HOST: u8 = 0x1;
                let svm_flags = if v.to_host { VMADDR_FLAG_TO_HOST } else { 0 };
                let addr = libc::sockaddr_vm {
                    svm_family: libc::AF_VSOCK as u16,
                    svm_reserved1: 0,
                    svm_port: v.port,
                    svm_cid: v.cid,
                    svm_zero: [svm_flags, 0, 0, 0],
                };
                assert!(std::mem::align_of::<libc::sockaddr_vm>() == 4);
                assert!(std::mem::size_of::<libc::sockaddr_vm>() == 16);

                // SAFETY: `addr` is repr(C) and fully initialzied;
                // connect() only reads within the size bounds given
                let r = libc::connect(
                    socket.as_raw_fd(),
                    &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_vm>() as _,
                );
                if r != 0 {
                    return Err(tag!(
                        "Failed to connnect to socket at {}: {}",
                        v.to_string(),
                        Errno::last()
                    ));
                }
                socket
            }
        }
        #[cfg(not(target_os = "linux"))]
        SocketSpec::VSock(_) => unreachable!(),
    };
    set_nonblock(&socket)?;
    Ok(socket)
}

/** Helper structure to unlink a created file when it Drops.
 *
 * This keeps the folder in which the file was created alive. */
struct FileCleanup {
    folder: OwnedFd,
    full_path: PathBuf,
}

impl Drop for FileCleanup {
    fn drop(&mut self) {
        let file_name = self.full_path.file_name().unwrap();
        debug!("Trying to unlink socket created at: {:?}", self.full_path);
        if let Err(x) = unistd::unlinkat(
            Some(self.folder.as_raw_fd()),
            file_name,
            unistd::UnlinkatFlags::NoRemoveDir,
        ) {
            error!(
                "Failed to unlink display socket at: {:?}: {:?}",
                self.full_path, x
            )
        }
    }
}

/** Create and bind to a Unix socket
 *
 * This returns the socket and a file cleanup structure to unlink it when
 * it is no longer needed.
 */
fn unix_socket_create_and_bind(
    path: &Path,
    cwd: &OwnedFd,
    flags: socket::SockFlag,
) -> Result<(OwnedFd, FileCleanup), String> {
    let socket: OwnedFd = socket::socket(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        flags,
        None,
    )
    .map_err(|x| tag!("Failed to create socket: {}", x))?;

    let file = path
        .file_name()
        .ok_or_else(|| tag!("Socket path {:?} missing file name", path))?;
    let addr = socket::UnixAddr::new(file)
        .map_err(|x| tag!("Failed to create Unix socket address from file name: {}", x))?;

    let (f, r) = if let Some(folder) = path.parent() {
        let f = open_folder(folder)?;

        unistd::fchdir(f.as_raw_fd()).map_err(|x| tag!("Failed to visit folder: {}", x))?;
        // eventually: is a 'bindat' equivalent available?
        // can use /proc/self/fd to workaround socket path length issues
        let x = socket::bind(socket.as_raw_fd(), &addr);
        unistd::fchdir(cwd.as_raw_fd())
            .map_err(|x| tag!("Failed to return to original path: {}", x))?;
        (f, x)
    } else {
        let f: OwnedFd =
            OwnedFd::try_clone(cwd).map_err(|x| tag!("Failed to duplicate cwd: {}", x))?;
        let x = socket::bind(socket.as_raw_fd(), &addr);
        (f, x)
    };
    r.map_err(|x| tag!("Failed to bind socket at {:?}: {}", path, x))?;
    Ok((
        socket,
        FileCleanup {
            folder: f,
            full_path: PathBuf::from(path),
        },
    ))
}

/** Create and bind to a socket */
fn socket_create_and_bind(
    path: &SocketSpec,
    cwd: &OwnedFd,
    flags: socket::SockFlag,
) -> Result<(OwnedFd, Option<FileCleanup>), String> {
    match path {
        #[cfg(target_os = "linux")]
        SocketSpec::VSock(spec) => {
            let socket: OwnedFd = socket::socket(
                socket::AddressFamily::Vsock,
                socket::SockType::Stream,
                flags,
                None,
            )
            .map_err(|x| tag!("Failed to create socket: {}", x))?;

            let addr = socket::VsockAddr::new(libc::VMADDR_CID_ANY, spec.port);
            socket::bind(socket.as_raw_fd(), &addr)
                .map_err(|x| tag!("Failed to bind socket at {}: {}", spec.to_string(), x))?;

            Ok((socket, None))
        }
        #[cfg(not(target_os = "linux"))]
        SocketSpec::VSock(_) => unreachable!(),

        SocketSpec::Unix(path) => {
            let (socket, cleanup) = unix_socket_create_and_bind(path, cwd, flags)?;
            Ok((socket, Some(cleanup)))
        }
    }
}

/** Connect to the Wayland display socket at the given `path` */
fn connect_to_display_at(cwd: &OwnedFd, path: &Path) -> Result<OwnedFd, String> {
    socket_connect(&SocketSpec::Unix(path.into()), cwd, false)
}

/** Connect to the Wayland display socket indicated by `WAYLAND_DISPLAY` and
 * (if not using absolute path) `XDG_RUNTIME_DIR` */
fn connect_to_wayland_display(cwd: &OwnedFd) -> Result<OwnedFd, String> {
    let wayl_disp = std::env::var_os("WAYLAND_DISPLAY")
        .ok_or("Missing environment variable WAYLAND_DISPLAY")?;
    let leading_slash: &[u8] = b"/";

    if wayl_disp.as_encoded_bytes().starts_with(leading_slash) {
        connect_to_display_at(cwd, Path::new(&wayl_disp))
    } else if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let mut path = PathBuf::new();
        path.push(dir);
        path.push(wayl_disp);
        connect_to_display_at(cwd, &path)
    } else {
        Err(tag!("XDG_RUNTIME_DIR was not in environment"))
    }
}

/** Get the socket fd number indicated by environment variable `WAYLAND_SOCKET` */
fn get_wayland_socket_id() -> Result<Option<i32>, String> {
    if let Some(x) = std::env::var_os("WAYLAND_SOCKET") {
        let y = x
            .into_string()
            .ok()
            .and_then(|x| x.parse::<i32>().ok())
            .ok_or("Failed to parse connection fd")?;
        Ok(Some(y))
    } else {
        Ok(None)
    }
}

/** For SIGINT handler; is set to true after SIGINT was received */
static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

/** Handler to record whether SIGINT was received */
extern "C" fn sigint_handler(_signo: i32) {
    SIGINT_RECEIVED.store(true, Ordering::Release);
}

/** Setup a SIGINT handler, and return a modified poll mask in which SIGINT is not blocked. */
fn setup_sigint_handler() -> Result<(signal::SigSet, &'static AtomicBool), String> {
    /* Block SIGINT, except when polling; this prevents a race in which SIGINT is received outside the poll. */
    let mut mask = signal::SigSet::empty();
    mask.add(signal::SIGINT);
    let mut pollmask = mask
        .thread_swap_mask(signal::SigmaskHow::SIG_BLOCK)
        .map_err(|x| tag!("Failed to set sigmask: {}", x))?;
    pollmask.remove(signal::SIGINT);

    let sigaction = signal::SigAction::new(
        signal::SigHandler::Handler(sigint_handler),
        signal::SaFlags::SA_NOCLDSTOP,
        signal::SigSet::empty(),
    );
    unsafe {
        // SAFETY: this is only called once, so existing signal handler is being overwritten;
        // Also, sigint_handler is async signal safe
        signal::sigaction(signal::Signal::SIGINT, &sigaction)
            .map_err(|x| tag!("Failed to set sigaction: {}", x))?;
    }

    Ok((pollmask, &SIGINT_RECEIVED))
}

/** Check connection header and run the main proxy loop for a new
 * `waypipe client` connection. */
fn handle_client_conn(link_fd: OwnedFd, wayland_fd: OwnedFd, opts: &Options) -> Result<(), String> {
    let mut buf = [0_u8; 16];
    let nr = nix::unistd::read(link_fd.as_raw_fd(), &mut buf)
        .map_err(|x| tag!("Failed to read connection header: {}", x))?;
    if nr != buf.len() {
        return Err(tag!("Connection header not completely sent ({} bytes)", nr));
    }

    let header = u32::from_le_bytes(buf[..4].try_into().unwrap());
    debug!("Connection header: 0x{:08x}", header);

    if header & CONN_FIXED_BIT == 0 && header & CONN_UNSET_BIT != 0 {
        error!("Possible endianness mismatch");
        return Err(tag!(
            "Header failure: possible endianness mismatch, or garbage input"
        ));
    }

    let version = (((header >> 16) & 0xff) << 4) | (header >> 3) & 0xf;
    let min_version = std::cmp::min(version, WAYPIPE_PROTOCOL_VERSION);
    debug!(
        "Connection remote version is {}, local is {}, using {}",
        version, WAYPIPE_PROTOCOL_VERSION, min_version
    );

    let comp = header & CONN_COMPRESSION_MASK;
    /* Validate compression type */
    let expected_comp = match opts.compression {
        Compression::None => CONN_NO_COMPRESSION,
        Compression::Lz4(_) => CONN_LZ4_COMPRESSION,
        Compression::Zstd(_) => CONN_ZSTD_COMPRESSION,
    };

    if comp != expected_comp {
        error!("Rejecting connection header {:x} due to compression type mismatch: header has {:x} != own {:x}", header, comp, expected_comp);
        return Err(tag!("Header compression failure"));
    }

    let video = header & CONN_VIDEO_MASK;
    /* Ignore video details for now? OpenDMAVidSrcV2/OpenDMAVidDstV2 explicitly specify format */
    match video {
        CONN_NO_VIDEO => {
            debug!("Connected waypipe-server not receiving video");
        }
        CONN_H264_VIDEO => {
            debug!("Connected waypipe-server may send H264 video");
        }
        CONN_VP9_VIDEO => {
            debug!("Connected waypipe-server may send VP9 video");
        }
        CONN_AV1_VIDEO => {
            debug!("Connected waypipe-server may send AV1 video");
        }
        _ => {
            debug!("Unknown video format specification")
        }
    }

    /* Nothing to do here: dmabuf creation requires letting linux-dmabuf-v1 through */
    let remote_using_dmabuf = header & CONN_NO_DMABUF_SUPPORT == 0;
    debug!(
        "Connected waypipe-server may use dmabufs: {}",
        remote_using_dmabuf
    );

    set_nonblock(&link_fd)?;
    set_nonblock(&wayland_fd)?;

    let (sigmask, sigint_received) = setup_sigint_handler()?;
    mainloop::main_interface_loop(
        link_fd,
        wayland_fd,
        opts,
        min_version,
        true,
        sigmask,
        sigint_received,
    )
}

/** Get filesystem path for waypipe (via /proc/self/exe) */
fn get_self_path() -> Result<OsString, String> {
    fcntl::readlink("/proc/self/exe").map_err(|x| {
        tag!(
            "Failed to lookup path to own executable (/proc/self/exe): {}",
            x
        )
    })
}

/** Start a process to handle a specific Wayland connection.
 *
 * (Use spawn instead of forking. Spawning ensures ASLR is reseeded, and
 * provides a nicer command line string, with the mixed-cost-benefit of
 * using the latest dynamic library versions, and repeating process
 * setup costs.) */
fn spawn_connection_handler(
    self_path: &OsStr,
    conn_args: &[&OsStr],
    wrapped_fd: OwnedFd,
    wayland_display: Option<&OsStr>,
) -> Result<Child, String> {
    /* Launch connection handler using explicit path to self.  An alternative
     * would be to directly use /proc/self/exe, but some tools would end up
     * using the file name 'exe' to label the client process, instead of the
     * value of argv0, which could be confusing. */
    let mut process = std::process::Command::new(self_path);
    process
        .arg0(env!("CARGO_PKG_NAME"))
        .args(conn_args)
        .env(
            "WAYPIPE_CONNECTION_FD",
            format!("{}", wrapped_fd.as_raw_fd()),
        )
        .env_remove("WAYLAND_SOCKET");
    if let Some(disp) = wayland_display {
        process.env("WAYLAND_DISPLAY", disp);
    }

    let child = process.spawn().map_err(|x| {
        tag!(
            "Failed to run connection subprocess with path {:?}: {}",
            self_path,
            x
        )
    })?;

    drop(wrapped_fd);

    Ok(child)
}

/** `waypipe-server` logic for the single connection case */
fn run_server_oneshot(
    command: &[&std::ffi::OsStr],
    argv0: &std::ffi::OsStr,
    options: &Options,
    unlink_at_end: bool,
    socket_path: &SocketSpec,
    cwd: &OwnedFd,
) -> Result<(), String> {
    let (sock1, sock2) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )
    .map_err(|x| tag!("Failed to create socketpair: {}", x))?;

    let sock_str = format!("{}", sock2.as_raw_fd());
    set_cloexec(&sock2, false)?;

    let mut cmd_child: std::process::Child = std::process::Command::new(command[0])
        .arg0(argv0)
        .args(&command[1..])
        .env("WAYLAND_SOCKET", &sock_str)
        .env_remove("WAYLAND_DISPLAY")
        .spawn()
        .map_err(|x| tag!("Failed to run program {:?}: {}", command[0], x))?;
    drop(sock2);

    let link_fd = socket_connect(socket_path, cwd, unlink_at_end)?;

    handle_server_conn(link_fd, sock1, options, None)?;

    debug!("Waiting for only child {} to reveal status", cmd_child.id());
    let _ = cmd_child.wait();
    debug!("Status received");

    Ok(())
}

/** Inner function for `run_server_multi`, used to ensure its cleanup always runs */
fn run_server_inner(
    display_socket: &OwnedFd,
    argv0: &std::ffi::OsStr,
    command: &[&OsStr],
    display_short: &OsStr,
    conn_args: &[&OsStr],
    connections: &mut BTreeMap<u32, std::process::Child>,
) -> Result<std::process::Child, String> {
    let mut cmd_child: std::process::Child = std::process::Command::new(command[0])
        .arg0(argv0)
        .args(&command[1..])
        .env("WAYLAND_DISPLAY", display_short)
        .env_remove("WAYLAND_SOCKET")
        .spawn()
        .map_err(|x| tag!("Failed to run program {:?}: {}", command[0], x))?;

    /* Block SIGCHLD _after_ spawning subprocess, to avoid having cmd_child inherit
     * signal disposition changes; note this process should be single threaded. */
    let mut mask = signal::SigSet::empty();
    mask.add(signal::SIGCHLD);
    let mut pollmask = mask
        .thread_swap_mask(signal::SigmaskHow::SIG_BLOCK)
        .map_err(|x| tag!("Failed to set sigmask: {}", x))?;
    pollmask.remove(signal::SIGCHLD);

    let sigaction = signal::SigAction::new(
        signal::SigHandler::Handler(noop_signal_handler),
        signal::SaFlags::SA_NOCLDSTOP,
        signal::SigSet::empty(),
    );
    unsafe {
        // SAFETY: signal handler installed is trivial
        signal::sigaction(signal::Signal::SIGCHLD, &sigaction)
            .map_err(|x| tag!("Failed to set sigaction: {}", x))?;
    }

    let self_path = get_self_path()?;

    socket::listen(&display_socket, socket::Backlog::MAXCONN)
        .map_err(|x| tag!("Failed to listen to socket: {}", x))?;
    'outer: loop {
        loop {
            /* Handle any child process exits */
            let res = wait::waitid(
                wait::Id::All,
                wait::WaitPidFlag::WEXITED
                    | wait::WaitPidFlag::WNOHANG
                    | wait::WaitPidFlag::WNOWAIT,
            );
            match res {
                Ok(status) => match status {
                    wait::WaitStatus::Exited(pid, _code) => {
                        if pid.as_raw() as u32 == cmd_child.id() {
                            let _ = cmd_child.wait();
                            debug!("Exiting, main command has stopped");
                            break 'outer;
                        }
                        prune_connections(connections, pid);
                    }
                    wait::WaitStatus::Signaled(pid, _signal, _bool) => {
                        if pid.as_raw() as u32 == cmd_child.id() {
                            let _ = cmd_child.wait();
                            debug!("Exiting, main command has stopped");
                            break 'outer;
                        }
                        prune_connections(connections, pid);
                    }
                    wait::WaitStatus::StillAlive => {
                        break;
                    }
                    _ => {
                        panic!("Unexpected process status: {:?}", status);
                    }
                },
                Err(Errno::ECHILD) => {
                    error!("Unexpected: no unwaited for children");
                    break 'outer;
                }
                Err(errno) => {
                    assert!(errno == Errno::EINTR);
                }
            }
        }

        /* Wait for SIGCHLD or action */
        let mut pfds = [PollFd::new(display_socket.as_fd(), PollFlags::POLLIN)];
        let res = nix::poll::ppoll(&mut pfds, None, Some(pollmask));
        if let Err(errno) = res {
            assert!(errno == Errno::EINTR || errno == Errno::EAGAIN);
            continue;
        }

        let rev = pfds[0].revents().unwrap();
        if rev.contains(PollFlags::POLLERR) {
            debug!("Exiting, socket error");
            break 'outer;
        }
        if !rev.contains(PollFlags::POLLIN) {
            continue;
        }
        /* We have a connection */
        debug!("Connection received");

        let res = socket::accept(display_socket.as_raw_fd());
        match res {
            Ok(conn_fd) => {
                let wrapped_fd = unsafe {
                    // SAFETY: freshly created file descriptor, exclusively captured here
                    OwnedFd::from_raw_fd(conn_fd)
                };

                let child = spawn_connection_handler(&self_path, conn_args, wrapped_fd, None)?;
                let cid = child.id();
                if connections.insert(cid, child).is_some() {
                    return Err(tag!("Pid reuse: {}", cid));
                }
            }
            Err(errno) => {
                assert!(errno != Errno::EBADF && errno != Errno::EINVAL);
                // This can fail for a variety of reasons, including OOM
                // and the connection being aborted
                debug!("Failed to receive connection");
            }
        }
    }

    Ok(cmd_child)
}

/** `waypipe-server` logic for the multiple connection case */
fn run_server_multi(
    command: &[&std::ffi::OsStr],
    argv0: &std::ffi::OsStr,
    options: &Options,
    unlink_at_end: bool,
    socket_path: &SocketSpec,
    display_short: &OsStr,
    display: &OsStr,
    cwd: &OwnedFd,
) -> Result<(), String> {
    let mut connections = BTreeMap::new();

    let mut conn_strings = Vec::new();
    let conn_args = build_connection_command(&mut conn_strings, socket_path, options, false, false);

    let (display_socket, sock_cleanup) = unix_socket_create_and_bind(
        &PathBuf::from(display),
        cwd,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )?;

    let res = run_server_inner(
        &display_socket,
        argv0,
        command,
        display_short,
        &conn_args,
        &mut connections,
    );
    /* Unlink the connection socket that was used */
    let sock_err = if unlink_at_end {
        if let SocketSpec::Unix(p) = socket_path {
            nix::unistd::unlink(p).map_err(|x| tag!("Failed to unlink socket: {}", x))
        } else {
            Ok(())
        }
    } else {
        Ok(())
    };
    /* Unlink the Wayland display socket that was created */
    drop(sock_cleanup);
    if let Err(err) = res {
        if let Err(e) = sock_err {
            error!("While cleaning up: {}", e);
        }
        return Err(err);
    }
    sock_err?;

    debug!("Shutting down");
    // Wait for subprocesses to
    wait_for_connnections(connections);
    debug!("Done");
    Ok(())
}

/** `waypipe-client` logic for the single connection case */
fn run_client_oneshot(
    command: Option<&[&std::ffi::OsStr]>,
    options: &Options,
    wayland_fd: OwnedFd,
    socket_path: &SocketSpec,
    cwd: &OwnedFd,
) -> Result<(), String> {
    let (channel_socket, sock_cleanup) =
        socket_create_and_bind(socket_path, cwd, socket::SockFlag::SOCK_CLOEXEC)?;

    socket::listen(&channel_socket, socket::Backlog::new(1).unwrap())
        .map_err(|x| tag!("Failed to listen to socket: {}", x))?;

    /* After the socket has been created, start ssh if necessary */
    let mut cmd_child: Option<std::process::Child> = None;
    if let Some(command_seq) = command {
        cmd_child = Some(
            std::process::Command::new(command_seq[0])
                .args(&command_seq[1..])
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("WAYLAND_SOCKET")
                .spawn()
                .map_err(|x| tag!("Failed to run program {:?}: {}", command_seq[0], x))?,
        );
    }
    let link_fd = loop {
        let res = socket::accept(channel_socket.as_raw_fd());
        match res {
            Ok(conn_fd) => {
                break unsafe {
                    // SAFETY: freshly created file descriptor, exclusively captured here
                    OwnedFd::from_raw_fd(conn_fd)
                };
            }
            Err(Errno::EINTR) => continue,
            Err(x) => {
                return Err(tag!("Failed to accept for socket: {}", x));
            }
        }
    };
    set_cloexec(&link_fd, true)?;

    /* Now that ssh has connected to the socket, it can safely be removed
     * from the file system */
    drop(sock_cleanup);

    handle_client_conn(link_fd, wayland_fd, options)?;

    if let Some(mut c) = cmd_child {
        debug!("Waiting for only child {} to reveal status", c.id());
        let _ = c.wait();
        debug!("Status received");
    }
    debug!("Done");

    Ok(())
}

/** No-op signal handler (used to ensure SIGCHLD interrupts poll) */
extern "C" fn noop_signal_handler(_: i32) {}

/** Inner function for `run_client_multi`, used to ensure its cleanup always runs */
fn run_client_inner(
    channel_socket: &OwnedFd,
    command: Option<&[&OsStr]>,
    conn_args: &[&OsStr],
    wayland_display: &OsStr,
    connections: &mut BTreeMap<u32, std::process::Child>,
) -> Result<Option<std::process::Child>, String> {
    socket::listen(&channel_socket, socket::Backlog::MAXCONN)
        .map_err(|x| tag!("Failed to listen to socket: {}", x))?;

    /* Only run ssh once the necessary socket to forward has been set up */
    let mut cmd_child: Option<std::process::Child> = None;
    if let Some(command_seq) = command {
        cmd_child = Some(
            std::process::Command::new(command_seq[0])
                .args(&command_seq[1..])
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("WAYLAND_SOCKET")
                .spawn()
                .map_err(|x| tag!("Failed to run program {:?}: {}", command_seq[0], x))?,
        );
    }

    /* Block SIGCHLD _after_ spawning subprocess, to avoid having cmd_child inherit
     * signal disposition changes; note this process should be single threaded. */
    let mut mask = signal::SigSet::empty();
    mask.add(signal::SIGCHLD);
    let mut pollmask = mask
        .thread_swap_mask(signal::SigmaskHow::SIG_BLOCK)
        .map_err(|x| tag!("Failed to set sigmask: {}", x))?;
    pollmask.remove(signal::SIGCHLD);

    let sigaction = signal::SigAction::new(
        signal::SigHandler::Handler(noop_signal_handler),
        signal::SaFlags::SA_NOCLDSTOP,
        signal::SigSet::empty(),
    );
    unsafe {
        // SAFETY: signal handler installed is trivial
        signal::sigaction(signal::Signal::SIGCHLD, &sigaction)
            .map_err(|x| tag!("Failed to set sigaction: {}", x))?;
    }

    /* Collect path to self now, instead of later, for use when spawning subprocesses.
     * (When the executable is deleted, /proc/self/exe is modified with a " (deleted)"
     * suffix, so identifying the executable path could fail.) */
    let self_path = get_self_path()?;

    'outer: loop {
        /* Handle any child process exits (including the case where cmd_child exited immediately)*/
        loop {
            let res = wait::waitid(
                wait::Id::All,
                wait::WaitPidFlag::WEXITED
                    | wait::WaitPidFlag::WNOHANG
                    | wait::WaitPidFlag::WNOWAIT,
            );
            match res {
                Ok(status) => match status {
                    wait::WaitStatus::Exited(pid, _code) => {
                        if let Some(ref mut c) = cmd_child {
                            if pid.as_raw() as u32 == c.id() {
                                let _ = c.wait();
                                debug!("Exiting, main command has stopped");
                                break 'outer;
                            }
                        }
                        prune_connections(connections, pid);
                    }
                    wait::WaitStatus::Signaled(pid, _signal, _bool) => {
                        if let Some(ref mut c) = cmd_child {
                            if pid.as_raw() as u32 == c.id() {
                                let _ = c.wait();
                                debug!("Exiting, main command has stopped");
                                break 'outer;
                            }
                        }
                        prune_connections(connections, pid);
                    }
                    wait::WaitStatus::StillAlive => {
                        break;
                    }
                    _ => {
                        panic!("Unexpected process status: {:?}", status);
                    }
                },
                Err(Errno::ECHILD) => {
                    /* no children to wait for; can happen if no command is run */
                    break;
                }
                Err(errno) => {
                    assert!(errno == Errno::EINTR);
                    break;
                }
            }
        }

        /* Wait for SIGCHLD or socket connection */
        let mut pfds = [PollFd::new(channel_socket.as_fd(), PollFlags::POLLIN)];
        let res = nix::poll::ppoll(&mut pfds, None, Some(pollmask));
        if let Err(errno) = res {
            assert!(errno == Errno::EINTR || errno == Errno::EAGAIN);
            continue;
        }

        let rev = pfds[0].revents().unwrap();
        if rev.contains(PollFlags::POLLERR) {
            debug!("Exiting, socket error");
            break 'outer;
        }
        if !rev.contains(PollFlags::POLLIN) {
            continue;
        }

        /* We have a connection */
        debug!("Connection received");

        let res = socket::accept(channel_socket.as_raw_fd());
        match res {
            Ok(conn_fd) => {
                // note: since no reconnection is done, we do not need to
                // keep track of a subprocess
                let wrapped_fd = unsafe {
                    // SAFETY: freshly created file descriptor, exclusively captured here
                    OwnedFd::from_raw_fd(conn_fd)
                };

                let child = spawn_connection_handler(
                    &self_path,
                    conn_args,
                    wrapped_fd,
                    Some(wayland_display),
                )?;
                let cid = child.id();
                if connections.insert(cid, child).is_some() {
                    return Err(tag!("Pid reuse: {}", cid));
                }
            }
            Err(errno) => {
                assert!(errno != Errno::EBADF && errno != Errno::EINVAL);
                // This can fail for a variety of reasons, including OOM
                // and the connection being aborted
                debug!("Failed to receive connection");
            }
        }
    }
    Ok(cmd_child)
}

/** `waypipe-client` logic for the multiple connection case */
fn run_client_multi(
    command: Option<&[&std::ffi::OsStr]>,
    options: &Options,
    socket_path: &SocketSpec,
    wayland_display: &OsStr,
    anti_staircase: bool,
    cwd: &OwnedFd,
) -> Result<(), String> {
    let mut conn_strings = Vec::new();
    let conn_args = build_connection_command(
        &mut conn_strings,
        socket_path,
        options,
        true,
        anti_staircase,
    );

    let (channel_socket, sock_cleanup) = socket_create_and_bind(
        socket_path,
        cwd,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )?;

    let mut connections = BTreeMap::new();
    let cmd_child = run_client_inner(
        &channel_socket,
        command,
        &conn_args,
        wayland_display,
        &mut connections,
    )?;
    drop(sock_cleanup);

    debug!("Shutting down");
    wait_for_connnections(connections);

    if let Some(mut child) = cmd_child {
        debug!(
            "Waiting for client command child {} to reveal status",
            child.id()
        );
        let _ = child.wait();
        debug!("Status received");
    }

    debug!("Done");
    Ok(())
}

/** Main logic for `waypipe client` */
fn run_client(
    command: Option<&[&std::ffi::OsStr]>,
    opts: &Options,
    oneshot: bool,
    socket_path: &SocketSpec,
    anti_staircase: bool,
    cwd: &OwnedFd,
    wayland_socket: Option<OwnedFd>,
    secctx: Option<&str>,
) -> Result<(), String> {
    if let Some(app_id) = secctx {
        let (wayland_disp, sock_cleanup, close_fd) = setup_secctx(cwd, app_id, wayland_socket)?;

        if oneshot {
            let c = connect_to_display_at(cwd, Path::new(&wayland_disp))?;
            drop(sock_cleanup);
            drop(close_fd);
            run_client_oneshot(command, opts, c, socket_path, cwd)
        } else {
            let r = run_client_multi(
                command,
                opts,
                socket_path,
                &wayland_disp,
                anti_staircase,
                cwd,
            );
            drop(close_fd);
            drop(sock_cleanup);
            r
        }
    } else if oneshot || wayland_socket.is_some() {
        let wayland_fd: OwnedFd = if let Some(s) = wayland_socket {
            s
        } else {
            connect_to_wayland_display(cwd)?
        };
        run_client_oneshot(command, opts, wayland_fd, socket_path, cwd)
    } else {
        let wayland_disp = std::env::var_os("WAYLAND_DISPLAY").ok_or_else(|| tag!("The environment variable WAYLAND_DISPLAY is not set, cannot connect to Wayland server."))?;
        run_client_multi(
            command,
            opts,
            socket_path,
            &wayland_disp,
            anti_staircase,
            cwd,
        )
    }
}

/** Connect to upstream Wayland compositor, create a security context,
 * and return the path to the security context's socket */
fn setup_secctx(
    cwd: &OwnedFd,
    app_id: &str,
    wayland_socket: Option<OwnedFd>,
) -> Result<(OsString, FileCleanup, OwnedFd), String> {
    let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR");
    let mut secctx_sock_path = PathBuf::from(xdg_runtime.as_deref().unwrap_or(OsStr::new("/tmp/")));
    secctx_sock_path.push(format!("waypipe-secctx-{}", std::process::id()));

    debug!(
        "Setting up security context socket at: {:?}",
        secctx_sock_path
    );

    let (sock, sock_cleanup) = unix_socket_create_and_bind(
        &secctx_sock_path,
        cwd,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )?;

    socket::listen(&sock, socket::Backlog::MAXCONN)
        .map_err(|x| tag!("Failed to listen to socket: {}", x))?;

    let wayland_conn = if let Some(s) = wayland_socket {
        s
    } else {
        connect_to_wayland_display(cwd)?
    };

    let flags = fcntl::fcntl(wayland_conn.as_raw_fd(), fcntl::FcntlArg::F_GETFL)
        .map_err(|x| tag!("Failed to get wayland socket flags: {}", x))?;
    let mut flags = fcntl::OFlag::from_bits(flags).unwrap();
    flags.remove(fcntl::OFlag::O_NONBLOCK);
    fcntl::fcntl(wayland_conn.as_raw_fd(), fcntl::FcntlArg::F_SETFL(flags))
        .map_err(|x| tag!("Failed to set wayland socket flags: {}", x))?;

    let (close_r, close_w) = unistd::pipe2(fcntl::OFlag::O_CLOEXEC | fcntl::OFlag::O_NONBLOCK)
        .map_err(|x| tag!("Failed to create pipe: {:?}", x))?;

    secctx::provide_secctx(wayland_conn, app_id, sock, close_r)?;

    debug!("Security context is ready");
    Ok((secctx_sock_path.into_os_string(), sock_cleanup, close_w))
}

/** Identify the index of the hostname in ssh_args, and detect
 * whether ssh will force pseudo-terminal allocation */
fn locate_openssh_cmd_hostname(ssh_args: &[&OsStr]) -> Result<(usize, bool), String> {
    /* Based on command line help for openssh 8.0 and 9.7 */
    // let fix_letters = b"46AaCfGgKkMNnqsTtVvXxYy";
    let arg_letters = b"BbcDEeFIiJLlmOopQRSWw";
    let mut dst_idx = 0;
    let mut allocates_pty = false;
    /* Note: a valid hostname never has a - prefix */
    while dst_idx < ssh_args.len() {
        let base_arg: &[u8] = ssh_args[dst_idx].as_encoded_bytes();
        if !base_arg.starts_with(b"-") {
            /* Not an argument, must be hostname */
            break;
        }
        if base_arg.len() == 1 {
            return Err(tag!("Failed to parse arguments after ssh: single '-'?"));
        }
        if base_arg == [b'-', b'-'] {
            /* No arguments after -- */
            dst_idx += 1;
            break;
        }
        // loop over letters; fix*  arg ( value )
        for i in 1..base_arg.len() {
            if arg_letters.contains(&base_arg[i]) {
                if i == base_arg.len() - 1 {
                    // Value is next argument
                    dst_idx += 1;
                } else {
                    // Value is tail of this arguent
                }
            } else if base_arg[i] == b't' {
                allocates_pty = true;
            } else if base_arg[i] == b'T' {
                allocates_pty = false;
            } else {
                /* All other letters interpreted as fixed arg letters */
            }
        }
        // Eat this argument
        dst_idx += 1;
    }
    if dst_idx >= ssh_args.len() || ssh_args[dst_idx].as_encoded_bytes().starts_with(b"-") {
        Err(tag!("Failed to locate ssh hostname in {:?}", ssh_args))
    } else {
        Ok((dst_idx, allocates_pty))
    }
}

#[test]
fn test_ssh_parsing() {
    let x: &[(&[&str], usize, bool)] = &[
        (&["-tlfoo", "host", "command"], 2, true),
        (&["-t", "-l", "foo", "host", "command"], 3, true),
        (&["host"], 0, false),
        (&["host", "-t"], 0, false),
        (&["-T", "--", "host"], 2, false),
        (&["-T", "-t", "--", "host"], 3, true),
    ];
    for entry in x {
        let y: Vec<&std::ffi::OsStr> = entry.0.iter().map(|s| OsStr::new(*s)).collect();
        let r = locate_openssh_cmd_hostname(&y);
        println!("{:?} > {:?}", entry.0, r);
        assert!(r == Ok((entry.1, entry.2)));
    }
}

// an available/unavailable list could be constructed if #[cfg] where to apply to expressions
const VERSION_STRING_CARGO: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nfeatures:",
    "\n  lz4: ",
    cfg!(feature = "lz4"),
    "\n  zstd: ",
    cfg!(feature = "zstd"),
    "\n  dmabuf: ",
    cfg!(feature = "dmabuf"),
    "\n  video: ",
    cfg!(feature = "video"),
);
pub const VERSION_STRING: &str = match option_env!("WAYPIPE_VERSION") {
    Some(x) => x,
    None => VERSION_STRING_CARGO,
};

/** Main entrypoint */
fn main() -> Result<(), String> {
    let command = Command::new(env!("CARGO_PKG_NAME"))
        .disable_help_subcommand(true)
        .subcommand_required(true)
        .help_expected(true)
        .flatten_help(false)
        .subcommand_help_heading("Modes")
        .subcommand_value_name("MODE")
        .about(
            "A proxy to remotely use Wayland protocol applications\n\
            Example: waypipe ssh user@server weston-terminal\n\
            See `man 1 waypipe` for detailed help.",
        )
        .next_line_help(false)
        .version(option_env!("WAYPIPE_VERSION").unwrap_or(VERSION_STRING));
    let command = command
        .subcommand(
            Command::new("ssh")
                .about("Wrap an ssh invocation to run Waypipe on both ends of the connection, and\nautomatically forward Wayland applications")
                .disable_help_flag(true)
                // collect all following arguments
                .arg(Arg::new("ssh_args").num_args(0..).trailing_var_arg(true).allow_hyphen_values(true).help("Arguments for ssh"))
        ).subcommand(
            Command::new("server")
            .about("Run remotely to run a process and forward application data through a socket\nto a matching `waypipe client` instance")
            .disable_help_flag(true)
            // collect all following arguments as the command
            .arg(Arg::new("command").num_args(0..).trailing_var_arg(true).help("Command to execute")
            .allow_hyphen_values(true) )
        ).subcommand(
            Command::new("client")
                .disable_help_flag(true)
                .about("Run locally to set up a Unix socket that `waypipe server` can connect to")
                // forbid all following arguments
        ).subcommand(
            Command::new("bench")
                .about("Estimate the best compression level used to send data, for each bandwidth")
                .disable_help_flag(true)
        ).subcommand(
            Command::new("server-conn")
                .disable_help_flag(true).hide(true)
        ).subcommand(
            Command::new("client-conn")
                .disable_help_flag(true).hide(true)
        );
    let command = command
        .arg(
            Arg::new("compress")
                .short('c')
                .long("compress")
                .value_name("comp")
                .help("Choose compression method: lz4[=#], zstd[=#], none")
                .value_parser(value_parser!(Compression))
                .default_value("lz4"),
        )
        .arg(
            Arg::new("debug")
                .short('d')
                .long("debug")
                .help("Print debug messages")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("no-gpu")
                .short('n')
                .long("no-gpu")
                .help("Block protocols using GPU memory transfers (via DMABUFs)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("oneshot")
                .short('o')
                .long("oneshot")
                .help("Only permit one connected application")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("socket")
                .short('s')
                .long("socket")
                .value_name("path")
                .help(
                    "Set the socket path to either create or connect to.\n\
                  - server default: /tmp/waypipe-server.sock\n\
                  - client default: /tmp/waypipe-client.sock\n\
                  - ssh: sets the prefix for the socket path\n\
                  - vsock: [[s]CID:]port",
                )
                // todo: decide value parser based on --vsock flag?
                .value_parser(value_parser!(OsString)),
        )
        .arg(
            Arg::new("display")
                .long("display")
                .value_name("display")
                .help("server,ssh: Set the Wayland display name or path")
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("drm-node")
                .long("drm-node")
                .value_name("path")
                .help("Set preferred DRM node (may be ignored in ssh/client modes)")
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("remote-node")
                .long("remote-node")
                .value_name("path")
                .help("ssh: Set the preferred remote DRM node")
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("remote-bin")
                .long("remote-bin")
                .value_name("path")
                .help("ssh: Set the remote Waypipe binary to use")
                .value_parser(value_parser!(PathBuf))
                .default_value(env!("CARGO_PKG_NAME")),
        )
        .arg(
            Arg::new("login-shell")
                .long("login-shell")
                .help("server: If server command is empty, run a login shell")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("threads")
                .long("threads")
                .help("Number of worker threads to use: 0 ⇒ hardware threads/2")
                .value_parser(value_parser!(u32))
                .default_value("0"),
        )
        .arg(
            Arg::new("title-prefix")
                .long("title-prefix")
                .value_name("str")
                .help("Prepend string to all window titles")
                .default_value(""),
        )
        .arg(
            Arg::new("unlink-socket")
                .long("unlink-socket")
                .help("server: Unlink the socket that Waypipe connects to")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("video")
                .long("video")
                .value_name("options")
                .help(
                    "Video-encode DMABUFs when possible\n\
                option format: (none|h264|vp9|av1)[,bpf=<X>]",
                )
                .default_value("none")
                .value_parser(value_parser!(VideoSetting)),
        )
        .arg(
            Arg::new("vsock")
                .long("vsock")
                .help("Connect over vsock-type socket instead of unix socket")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("secctx")
                .long("secctx")
                .value_name("str")
                .help("client,ssh: Use security-context protocol with application ID")
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("anti-staircase")
                .long("anti-staircase")
                .hide(true)
                .help("Prevent staircasing effect in terminal logs")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("test-loop")
                .long("test-loop")
                .hide(true)
                .help("Test option: act like `ssh localhost` without ssh")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("test-wire-version")
            .long("test-wire-version")
            .hide(true)
            .help("Test option: set the wire protocol version tried for `waypipe server`; must be >= 16")
            .value_parser(value_parser!(u32)),
        )
        .arg(
            Arg::new("test-fast-bench")
                .long("test-fast-bench")
                .hide(true)
                .help("Test option: run 'bench' mode on a tiny input")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("trace")
                .long("trace")
                .action(ArgAction::SetTrue)
                .help("Test option: log all Wayland messages received and sent")
                .hide(true),
        );
    let matches = command.get_matches();

    let debug = matches.get_one::<bool>("debug").unwrap();
    let trace = matches.get_one::<bool>("trace").unwrap();

    let max_level = if *trace {
        log::LevelFilter::Trace
    } else if *debug {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Error
    };

    let (log_color, log_label) = match matches.subcommand() {
        Some(("ssh", _)) => (1, "waypipe-client"),
        Some(("client", _)) => (1, "waypipe-client"),
        Some(("server", _)) => (2, "waypipe-server"),
        Some(("client-conn", _)) => (1, "waypipe-client"),
        Some(("server-conn", _)) => (2, "waypipe-server"),
        _ => (0, "waypipe"),
    };

    let mut anti_staircase: bool = *matches.get_one::<bool>("anti-staircase").unwrap();
    if let Some(("ssh", submatch)) = matches.subcommand() {
        let subargs = submatch.get_raw("ssh_args");
        let ssh_args: Vec<&std::ffi::OsStr> = subargs.unwrap_or_default().collect();
        let (destination_idx, allocates_pty) = locate_openssh_cmd_hostname(&ssh_args)?;
        let needs_login_shell = destination_idx == ssh_args.len() - 1;
        anti_staircase = needs_login_shell || allocates_pty;
    }
    let logger = Logger {
        max_level,
        pid: std::process::id(),
        color_output: nix::unistd::isatty(2).unwrap(),
        anti_staircase,
        color: log_color,
        label: log_label,
    };

    log::set_max_level(max_level);
    log::set_boxed_logger(Box::new(logger)).unwrap();

    let mut oneshot = *matches.get_one::<bool>("oneshot").unwrap();
    let no_gpu = matches.get_one::<bool>("no-gpu").unwrap();
    let remotebin = matches.get_one::<PathBuf>("remote-bin").unwrap();
    let socket_arg = matches.get_one::<OsString>("socket");
    let title_prefix = matches.get_one::<String>("title-prefix").unwrap();
    let display = matches.get_one::<PathBuf>("display");
    let threads = matches.get_one::<u32>("threads").unwrap();
    let unlink = *matches.get_one::<bool>("unlink-socket").unwrap();
    let mut compression: Compression = *matches.get_one::<Compression>("compress").unwrap();
    let mut video: VideoSetting = *matches.get_one::<VideoSetting>("video").unwrap();
    let login_shell = matches.get_one::<bool>("login-shell").unwrap();
    let remote_node = matches.get_one::<PathBuf>("remote-node");
    let drm_node = matches.get_one::<PathBuf>("drm-node");
    let loop_test = matches.get_one::<bool>("test-loop").unwrap();
    let fast_bench = *matches.get_one::<bool>("test-fast-bench").unwrap();
    let test_wire_version: Option<u32> = matches.get_one::<u32>("test-wire-version").copied();
    let secctx = matches.get_one::<String>("secctx");
    let vsock = *matches.get_one::<bool>("vsock").unwrap();

    if !oneshot && std::env::var_os("WAYLAND_SOCKET").is_some() {
        debug!("Automatically enabling oneshot mode because WAYLAND_SOCKET is present");
        oneshot = true;
    }

    if cfg!(not(feature = "video")) && video.format.is_some() {
        error!("Waypipe was not build with video encoding support, ignoring --video command line option.");
        video.format = None;
    }

    if cfg!(not(target_os = "linux")) && vsock {
        return Err(
            "Waypipe was built with support for VSOCK-type sockets on this platform.".into(),
        );
    }
    if vsock && socket_arg.is_none() {
        return Err("Socket must be specified with --socket when --vsock option used".into());
    }
    let socket: Option<SocketSpec> = if let Some(s) = socket_arg {
        if vsock {
            Some(SocketSpec::VSock(VSockConfig::from_str(
                s.to_str().unwrap(),
            )?))
        } else {
            Some(SocketSpec::Unix(PathBuf::from(s)))
        }
    } else {
        None
    };

    if let Compression::Lz4(_) = compression {
        if cfg!(not(feature = "lz4")) {
            error!("Waypipe was not built with lz4 compression/decompression support, downgrading compression mode to 'none'");
            compression = Compression::None;
        }
    }
    if let Compression::Zstd(_) = compression {
        if cfg!(not(feature = "zstd")) {
            error!("Waypipe was not built with zstd compression/decompression support, downgrading compression mode to 'none'");
            compression = Compression::None;
        }
    }

    let opts = Options {
        debug: *debug,
        no_gpu: *no_gpu || cfg!(not(feature = "dmabuf")),
        compression,
        video,
        threads: *threads,
        title_prefix: (*title_prefix).clone(),
        drm_node: drm_node.cloned(),
    };

    /* Needed to revert back to original cwdir after
     * changes made to possibly open sockets with a >108 byte path length. */
    let cwd: OwnedFd = open_folder(&PathBuf::from("."))?;

    /* If WAYLAND_SOCKET was set, extract the socket now, ensuring it is only done once */
    let wayland_socket = if let Some(wayl_sock) = get_wayland_socket_id()? {
        let fd = unsafe {
            // SAFETY: relies on external promise that value is a Wayland connection fd,
            // and not something else (like of STDIN/STDOUT/STDRRR). Exclusive, because
            // this is the only place WAYLAND_SOCKET is extracted
            OwnedFd::from_raw_fd(RawFd::from(wayl_sock))
        };
        /* Ensure spawned processes cannot inherit the socket. (All spawned processes have
         * the environment variable WAYLAND_SOCKET cleared, so cannot detect it.) */
        set_cloexec(&fd, true)?;
        Some(fd)
    } else {
        None
    };

    debug!(
        "waypipe version: {}",
        VERSION_STRING.split_once('\n').unwrap().0
    );
    match matches.subcommand() {
        Some(("ssh", submatch)) => {
            debug!("Starting client+ssh main process");
            let subargs = submatch.get_raw("ssh_args");
            let ssh_args: Vec<&std::ffi::OsStr> = subargs.unwrap_or_default().collect();
            let (destination_idx, _) = locate_openssh_cmd_hostname(&ssh_args)?;
            /* Login shell required if there are no arguments following the ssh destination */
            let needs_login_shell = destination_idx == ssh_args.len() - 1;

            let socket_path: SocketSpec =
                socket.unwrap_or(SocketSpec::Unix(PathBuf::from("/tmp/waypipe")));

            let rand_tag = get_rand_tag()?;
            let mut client_sock_path = OsString::new();
            let mut server_sock_path = OsString::new();
            let client_sock = match socket_path {
                SocketSpec::Unix(path) => {
                    client_sock_path.push(&path);
                    client_sock_path.push(OsStr::new("-client-"));
                    client_sock_path.push(OsStr::from_bytes(&rand_tag));
                    client_sock_path.push(OsStr::new(".sock"));
                    server_sock_path.push(&path);
                    server_sock_path.push(OsStr::new("-server-"));
                    server_sock_path.push(OsStr::from_bytes(&rand_tag));
                    server_sock_path.push(OsStr::new(".sock"));
                    if *loop_test {
                        let client_path = PathBuf::from(&client_sock_path);
                        let server_path = PathBuf::from(&server_sock_path);
                        unistd::symlinkat(&client_path, None, &server_path).map_err(|x| {
                            tag!(
                                "Failed to create symlink from {:?} to {:?}: {}",
                                client_path,
                                server_path,
                                x
                            )
                        })?;
                    }
                    SocketSpec::Unix(PathBuf::from(client_sock_path.clone()))
                }
                SocketSpec::VSock(v) => {
                    server_sock_path = OsString::from(v.port.to_string());
                    SocketSpec::VSock(v)
                }
            };
            let mut linkage = OsString::new();
            linkage.push(server_sock_path.clone());
            linkage.push(OsStr::new(":"));
            linkage.push(client_sock_path.clone());
            let mut wayland_display = OsString::new();
            if let Some(p) = display {
                wayland_display.push(p);
            } else {
                wayland_display.push(OsStr::new("wayland-"));
                wayland_display.push(OsStr::from_bytes(&rand_tag));
            }

            let mut ssh_cmd: Vec<&std::ffi::OsStr> = Vec::new();
            if !loop_test {
                ssh_cmd.push(OsStr::new("ssh"));
                if needs_login_shell {
                    ssh_cmd.push(OsStr::new("-t"));
                }
                if matches!(client_sock, SocketSpec::Unix(_)) {
                    ssh_cmd.push(OsStr::new("-R"));
                    ssh_cmd.push(&linkage);
                }
                ssh_cmd.extend_from_slice(&ssh_args[..=destination_idx]);
            }
            ssh_cmd.push(OsStr::new(remotebin));
            if opts.debug {
                ssh_cmd.push(OsStr::new("--debug"));
            }
            if oneshot {
                ssh_cmd.push(OsStr::new("--oneshot"));
            }
            if needs_login_shell {
                ssh_cmd.push(OsStr::new("--login-shell"));
            }
            if opts.no_gpu {
                ssh_cmd.push(OsStr::new("--no-gpu"));
            }
            ssh_cmd.push(OsStr::new("--unlink-socket"));
            ssh_cmd.push(OsStr::new("--threads"));
            let arg_nthreads = OsString::from(opts.threads.to_string());
            ssh_cmd.push(&arg_nthreads);

            let arg_drm_node_val;
            if let Some(r) = remote_node {
                arg_drm_node_val = r.clone().into_os_string();
                ssh_cmd.push(OsStr::new("--drm-node"));
                ssh_cmd.push(&arg_drm_node_val);
            }

            ssh_cmd.push(OsStr::new("--compress"));
            let arg_compress_val = OsString::from(compression.to_string());
            ssh_cmd.push(&arg_compress_val);
            let arg_video = OsString::from(format!("--video={}", video));
            if video.format.is_some() {
                ssh_cmd.push(&arg_video);
            }

            if matches!(client_sock, SocketSpec::VSock(_)) {
                ssh_cmd.push(OsStr::new("--vsock"));
            }
            ssh_cmd.push(OsStr::new("--socket"));
            ssh_cmd.push(&server_sock_path);
            if !oneshot {
                ssh_cmd.push(OsStr::new("--display"));
                ssh_cmd.push(&wayland_display);
            }
            ssh_cmd.push(OsStr::new("server"));
            ssh_cmd.extend_from_slice(&ssh_args[destination_idx + 1..]);

            run_client(
                Some(&ssh_cmd),
                &opts,
                oneshot,
                &client_sock,
                anti_staircase,
                &cwd,
                wayland_socket,
                secctx.map(|x| x.as_str()),
            )
        }
        Some(("client", _submatch)) => {
            debug!("Starting client main process");
            let socket_path: SocketSpec =
                socket.unwrap_or(SocketSpec::Unix(PathBuf::from("/tmp/waypipe-client.sock")));

            run_client(
                None,
                &opts,
                oneshot,
                &socket_path,
                false,
                &cwd,
                wayland_socket,
                secctx.map(|x| x.as_str()),
            )
        }
        Some(("server", submatch)) => {
            debug!("Starting server main process");
            let subargs = submatch.get_raw("command");

            let (shell, shell_argv0) = if let Some(shell) = std::env::var_os("SHELL") {
                let bt = shell.as_bytes();
                let mut a = OsString::new();
                a.push("-");
                if let Some(idx) = bt.iter().rposition(|x| *x == b'/') {
                    let sl: &[u8] = &bt[idx + 1..];
                    a.push(OsStr::from_bytes(sl));
                } else {
                    a.push(shell.clone());
                };
                (shell.clone(), a)
            } else {
                (OsString::from("/bin/sh"), OsString::from("-sh"))
            };

            let (command, argv0): (Vec<&std::ffi::OsStr>, &std::ffi::OsStr) =
                if let Some(s) = subargs {
                    let x: Vec<_> = s.collect();
                    let y: &std::ffi::OsStr = x[0];
                    (x, y)
                } else {
                    let sv: Vec<&std::ffi::OsStr> = vec![&shell];
                    (sv, &shell_argv0)
                };
            let argv0 = if *login_shell { argv0 } else { command[0] };

            let socket_path: SocketSpec =
                socket.unwrap_or(SocketSpec::Unix(PathBuf::from("/tmp/waypipe-server.sock")));

            if oneshot {
                run_server_oneshot(&command, argv0, &opts, unlink, &socket_path, &cwd)
            } else {
                let display_val: PathBuf = if let Some(s) = display {
                    s.clone()
                } else {
                    let rand_tag = get_rand_tag()?;
                    let mut w = OsString::from("wayland-");
                    w.push(OsStr::from_bytes(&rand_tag));
                    PathBuf::from(w)
                };
                let display_path: PathBuf = if display_val.is_absolute() {
                    display_val.clone()
                } else {
                    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                        .ok_or_else(|| tag!("Environment variable XDG_RUNTIME_DIR not present"))?;
                    PathBuf::from(runtime_dir).join(&display_val)
                };

                run_server_multi(
                    &command,
                    argv0,
                    &opts,
                    unlink,
                    &socket_path,
                    display_val.as_ref(),
                    display_path.as_ref(),
                    &cwd,
                )
            }
        }
        Some(("bench", _)) => bench::run_benchmark(&opts, fast_bench),
        Some(("server-conn", _)) => {
            debug!("Starting server connection process");

            let env_sock = std::env::var_os("WAYPIPE_CONNECTION_FD")
                .ok_or_else(|| tag!("Connection fd not provided for server-conn mode"))?;
            let opt_fd = env_sock
                .into_string()
                .ok()
                .and_then(|x| x.parse::<i32>().ok())
                .ok_or_else(|| tag!("Failed to parse connection fd"))?;

            // Capture inherited fd from environment variable
            // Must only call connect_to_upstream() once, at risk of closing random fd
            let wayland_fd = unsafe {
                // SAFETY: relies on internal promise that value is a Wayland connection fd,
                // and not a random integer. Exclusive, because this is the only time
                // WAYPIPE_CONNECTION_FD is read from in this branch
                OwnedFd::from_raw_fd(RawFd::from(opt_fd))
            };

            set_cloexec(&wayland_fd, true)?;

            let link_fd = if let Some(s) = wayland_socket {
                /* For test use: provide the upstream Waypipe connection through WAYLAND_SOCKET.
                 * (To reduce the risk of unexpected connections, the downstream Wayland connection
                 * used WAYPIPE_CONNECTION_FD.) */
                s
            } else {
                socket_connect(
                    &socket.ok_or_else(|| tag!("Socket path not provided"))?,
                    &cwd,
                    false,
                )?
            };

            handle_server_conn(link_fd, wayland_fd, &opts, test_wire_version)
        }
        Some(("client-conn", _)) => {
            debug!("Starting client connection process");

            let env_sock = std::env::var_os("WAYPIPE_CONNECTION_FD")
                .ok_or_else(|| tag!("Connection fd not provided for client-conn mode"))?;
            let opt_fd = env_sock
                .into_string()
                .ok()
                .and_then(|x| x.parse::<i32>().ok())
                .ok_or("Failed to parse connection fd")?;
            let link_fd = unsafe {
                // SAFETY: relies on internal promise that value is a Wayland connection fd,
                // and not a random integer. Exclusive, because this is the only time
                // WAYPIPE_CONNECTION_FD is read from in this branch
                OwnedFd::from_raw_fd(RawFd::from(opt_fd))
            };

            let wayland_fd = if let Some(s) = wayland_socket {
                /* For test use: provide the Wayland connection through WAYLAND_SOCKET */
                s
            } else {
                connect_to_wayland_display(&cwd)?
            };
            debug!("have read initial bytes");

            handle_client_conn(link_fd, wayland_fd, &opts)
        }
        _ => unreachable!(),
    }
}
