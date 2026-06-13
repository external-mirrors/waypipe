/* SPDX-License-Identifier: GPL-3.0-or-later */
#![no_main]
/*! Fuzzing hook which runs the main loop against well formed Wayland protocol
 * data, using only regular shared memory and pipe file descriptors.
 *
 * DMABUFs and sync files are explicitly not included because graphics drivers and
 * video codecs can make fuzzing even slower and risk exhausting VRAM or triggering
 * driver bugs. */

use libfuzzer_sys::fuzz_target;

use log::{LevelFilter, Log, Record};
use mainloop::util::{Compression, VideoSetting, MIN_PROTOCOL_VERSION, WAYPIPE_PROTOCOL_VERSION};
use mainloop::*;
use nix::errno::Errno;
use nix::fcntl;
use nix::sys::memfd;
use nix::sys::resource;
use nix::sys::signal;
use nix::sys::socket;
use nix::sys::stat;
use nix::sys::uio;
use nix::{poll, unistd};
use std::collections::VecDeque;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::AtomicBool;
use std::thread;

const DUMMY_OBJECT_ID: u32 = 0xfffffff;
const WL_DISPLAY_ID: u32 = 1;
const WL_DISPLAY_ERROR_OPCODE: u8 = 0;

struct ThreadLogger;

impl Log for ThreadLogger {
    fn enabled(&self, _meta: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &Record<'_>) {
        use std::io::Write;

        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        let t = if let Ok(t) = time {
            (t.as_nanos() % 100000000000u128) / 1000u128
        } else {
            0
        };

        let thread = std::thread::current();

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
        let _ = writeln!(
            &mut cursor,
            "[{:02}.{:06} {} {}({}) {}:{}] {}",
            t / 1000000u128,
            t % 1000000u128,
            lvl_str,
            thread.name().unwrap_or("unknown"),
            unistd::gettid(),
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.args(),
        );
        let mut str_end = cursor.position() as usize;

        if str_end >= MAX_LOG_LEN - 10 {
            /* Deal with possible partial UTF-8 char */
            str_end = match std::str::from_utf8(&buf[..str_end]) {
                Ok(x) => x.len(),
                Err(y) => y.valid_up_to(),
            };

            /* Assume message was truncated */
            assert!(str_end <= MAX_LOG_LEN - 4, "{} {}", str_end, MAX_LOG_LEN);
            buf[str_end..str_end + 3].fill(b'.');
            buf[str_end + 3] = b'\n';
            str_end += 4;
        }
        let handle = &mut std::io::stderr().lock();
        handle.write_all(&buf[..str_end]).expect("stderr is OK");
        handle.flush().expect("stderr is OK");
    }
    fn flush(&self) {
        /* not needed */
    }
}

fn end_message() -> [u8; 8] {
    let a = DUMMY_OBJECT_ID.to_le_bytes();
    let b = (8u32 << 16).to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]]
}

fn parse_header(msg: &[u8]) -> (u32, usize, u8) {
    let obj_id = u32::from_le_bytes(msg[0..4].try_into().unwrap());
    let x = u32::from_le_bytes(msg[4..8].try_into().unwrap());
    let length = (x >> 16) as usize;
    let opcode = (x & 0x00ff) as u8;
    (obj_id, length, opcode)
}

#[derive(Default)]
struct SideState {
    wayland_bytes: Vec<u8>,
    wayland_fds: Vec<OwnedFd>,

    pipes: VecDeque<OwnedFd>,
    shm: VecDeque<OwnedFd>,
}

/** Write a batch of messages with associated FDs to `src_fd`, followed by
 * a message that should pass either through cleanly, or end with an error
 * directed to the program.
 *
 * This returns the FDs read from `prog_fd` and from `comp_fd`, respectively
 * on success; if both endpoints close this returns Err(());
 */
fn interact(
    prog_fd: &OwnedFd,
    comp_fd: &OwnedFd,
    write_to_prog: bool,
    data: &[u8],
    mut fds: &[&OwnedFd],
) -> Result<(Vec<OwnedFd>, Vec<OwnedFd>), ()> {
    let end_msg = end_message();

    let mut nbytes_sent: usize = 0;
    let mut nfds_sent: usize = 0;
    let net_len = data.len() + end_msg.len();

    let raw_fds: Vec<i32> = fds.iter().map(|x| x.as_raw_fd()).collect();

    /* Note: libwayland sends up to 28 fds per byte, Waypipe can receive a bit more.
     * Drop any excess file descriptors.
     */
    const MAX_FDS_PER_BYTE: usize = 32;
    fds = &fds[..fds.len().min(data.len().saturating_mul(MAX_FDS_PER_BYTE))];

    struct ReadState {
        data: Vec<u8>,
        rmsgs: Vec<Vec<u8>>,
        fds: Vec<OwnedFd>,
        eof: bool,
    }

    let mut recv_prog = ReadState {
        data: Vec::new(),
        fds: Vec::new(),
        rmsgs: Vec::new(),
        eof: false,
    };
    let mut recv_comp = ReadState {
        data: Vec::new(),
        fds: Vec::new(),
        rmsgs: Vec::new(),
        eof: false,
    };

    loop {
        let mut pfds = Vec::new();
        let mut recvs: Vec<&mut ReadState> = Vec::new();
        let writing = nbytes_sent < net_len;
        if !recv_prog.eof {
            pfds.push(poll::PollFd::new(
                prog_fd.as_fd(),
                if writing && write_to_prog {
                    poll::PollFlags::POLLIN | poll::PollFlags::POLLOUT
                } else {
                    poll::PollFlags::POLLIN
                },
            ));
            recvs.push(&mut recv_prog);
        }
        if !recv_comp.eof {
            pfds.push(poll::PollFd::new(
                comp_fd.as_fd(),
                if writing && !write_to_prog {
                    poll::PollFlags::POLLIN | poll::PollFlags::POLLOUT
                } else {
                    poll::PollFlags::POLLIN
                },
            ));
            recvs.push(&mut recv_comp);
        }

        let res = nix::poll::ppoll(&mut pfds, None, None);
        if let Err(e) = res {
            assert!(e == Errno::EINTR || e == Errno::EAGAIN);
        }

        for (pfd, recv) in pfds.into_iter().zip(recvs) {
            let evt = pfd.revents().unwrap();

            if evt.contains(poll::PollFlags::POLLIN) {
                assert!(!recv.eof);
                let mut tmp = vec![0u8; 16384];
                let mut iovs = [IoSliceMut::new(&mut tmp)];
                let mut cmsg_fds = nix::cmsg_space!([RawFd; 32]);

                let r = socket::recvmsg::<socket::UnixAddr>(
                    pfd.as_fd().as_raw_fd(),
                    &mut iovs,
                    Some(&mut cmsg_fds),
                    socket::MsgFlags::empty(),
                );
                match r {
                    Ok(resp) => {
                        for msg in resp.cmsgs().unwrap() {
                            match msg {
                                socket::ControlMessageOwned::ScmRights(tfds) => {
                                    for f in &tfds {
                                        assert!(*f != -1);
                                        recv.fds.push(unsafe {
                                            // SAFETY: fd was just created, checked valid,
                                            // and is recorded nowhere else
                                            OwnedFd::from_raw_fd(*f)
                                        });
                                    }
                                }
                                _ => {
                                    panic!("Unexpected control message");
                                }
                            }
                        }

                        let nbytes = resp.bytes;
                        recv.data.extend_from_slice(&tmp[..nbytes]);

                        recv.eof = nbytes == 0;
                    }
                    Err(nix::errno::Errno::ECONNRESET) => {
                        recv.eof = true;
                    }
                    Err(nix::errno::Errno::EINTR) | Err(nix::errno::Errno::EAGAIN) => (),
                    Err(x) => {
                        panic!("Error reading from socket: {:?}", x)
                    }
                }

                /* Extract any complete messages from stream */
                let mut tail: &[u8] = &recv.data;
                while tail.len() >= 8 {
                    let (_obj_id, length, _opcode) = parse_header(tail);
                    /* Waypipe should only ever write valid messages */
                    assert!(length >= 8 && length % 4 == 0);
                    if tail.len() >= length {
                        let (msg, nxt) = tail.split_at(length);
                        recv.rmsgs.push(msg.into());
                        tail = nxt;
                    }
                }
                recv.data.drain(..(recv.data.len() - tail.len()));
            } else if evt.intersects(poll::PollFlags::POLLHUP | poll::PollFlags::POLLERR) {
                recv.eof = true;
            }

            if evt.contains(poll::PollFlags::POLLOUT) {
                /* Only the writing side checks for POLLOUT, so write messages here */
                let iovs_long = [
                    IoSlice::new(&data[std::cmp::min(nbytes_sent, data.len())..]),
                    IoSlice::new(&end_msg[(std::cmp::max(nbytes_sent, data.len()) - data.len())..]),
                ];
                let iovs_short = [IoSlice::new(if nbytes_sent >= data.len() {
                    &end_msg[nbytes_sent - data.len()..(nbytes_sent - data.len() + 1)]
                } else {
                    &data[nbytes_sent..(nbytes_sent + 1)]
                })];
                let sfds =
                    &raw_fds[nfds_sent..std::cmp::min(fds.len(), nfds_sent + MAX_FDS_PER_BYTE)];
                let short_transfer = fds.len() > nfds_sent + MAX_FDS_PER_BYTE;
                let iovs: &[IoSlice] = if short_transfer {
                    &iovs_short
                } else {
                    &iovs_long
                };
                let cmsgs = [socket::ControlMessage::ScmRights(sfds)];

                let r = nix::sys::socket::sendmsg::<()>(
                    pfd.as_fd().as_raw_fd(),
                    iovs,
                    if sfds.is_empty() { &[] } else { &cmsgs },
                    nix::sys::socket::MsgFlags::empty(),
                    None,
                );
                match r {
                    Ok(s) => {
                        nbytes_sent += s;
                        nfds_sent += sfds.len();
                    }
                    Err(Errno::EINTR) | Err(Errno::EAGAIN) => {
                        println!("eintr");
                    }
                    Err(Errno::EPIPE) | Err(Errno::ECONNRESET) => {
                        recv.eof = true;
                    }
                    Err(e) => {
                        panic!("{:?}", e);
                    }
                }
            }
        }

        if recv_comp.eof && recv_prog.eof {
            return Err(());
        }

        let opp_recv = if write_to_prog {
            &mut recv_comp
        } else {
            &mut recv_prog
        };

        let mut done = false;
        for msg in opp_recv.rmsgs.iter() {
            if msg == &end_msg {
                done = true;
            }
        }

        for msg in &recv_prog.rmsgs {
            let (obj_id, _length, opcode) = parse_header(msg);
            if obj_id == WL_DISPLAY_ID && opcode == WL_DISPLAY_ERROR_OPCODE {
                done = true;
            }
        }

        if done {
            break;
        }
    }

    Ok((recv_prog.fds, recv_comp.fds))
}

fn is_pipe(fd: &OwnedFd) -> bool {
    let info = stat::fstat(fd).unwrap();
    (info.st_mode & stat::SFlag::S_IFMT.bits()) == stat::SFlag::S_IFIFO.bits()
}

fn is_shm(fd: &OwnedFd) -> bool {
    let info = stat::fstat(fd).unwrap();
    (info.st_mode & stat::SFlag::S_IFMT.bits()) == stat::SFlag::S_IFREG.bits()
}

fn make_shm(len: usize) -> OwnedFd {
    let local_fd = memfd::memfd_create(
        c"/waypipe",
        memfd::MFdFlags::MFD_CLOEXEC | memfd::MFdFlags::MFD_ALLOW_SEALING,
    )
    .unwrap();

    let data: Vec<u8> = (0..len).map(|x| if x % 2 != 0 { 255 } else { 0 }).collect();
    let b = unistd::write(&local_fd, &data).unwrap();
    assert!(b == data.len());

    local_fd
}

fn make_pipe() -> (OwnedFd, OwnedFd) {
    unistd::pipe2(fcntl::OFlag::O_NONBLOCK | fcntl::OFlag::O_CLOEXEC).unwrap()
}

fn get_shm_len(fd: &OwnedFd) -> usize {
    let info = stat::fstat(fd).unwrap();
    info.st_size.try_into().unwrap()
}

fn run_endpoints(disp_end: OwnedFd, prog_end: OwnedFd, mut data: &[u8]) {
    /* Interpret `data` as a series of chunks that look like Wayland
     * protocol messages; and send them alternatingly to the Waypipe
     * client and server threads, followed by a dummy message used to
     * confirm the queue has been flushed. */

    let mut disp_side = SideState::default();
    let mut prog_side = SideState::default();

    let mut scratch = [0u8; 10000];

    /* The first messages should always come from the client */
    let mut last_disp_side = true;
    loop {
        let Some((code, tail)) = data.split_first_chunk::<4>() else {
            return;
        };
        data = tail;
        let mut code = u32::from_le_bytes(*code);
        let is_disp_side = (code & 0x1) != 0;
        code >>= 1;

        /* Each action only affects a single side, and when the side changes
         * all actions are flushed.
         *
         * The side transition behavior should also help synchronize (and make a
         * bit more reproducible) fd operations on both sides of the connection */
        if is_disp_side != last_disp_side {
            let send_side = if last_disp_side {
                &mut disp_side
            } else {
                &mut prog_side
            };

            let fd_refs: Vec<&OwnedFd> = send_side.wayland_fds.iter().collect();

            let Ok((prog_fds, disp_fds)) = interact(
                &prog_end,
                &disp_end,
                !last_disp_side,
                &send_side.wayland_bytes,
                &fd_refs,
            ) else {
                return;
            };

            send_side.wayland_bytes.clear();
            send_side.wayland_fds.clear();

            for fd in prog_fds {
                if is_shm(&fd) {
                    prog_side.shm.push_back(fd);
                } else if is_pipe(&fd) {
                    prog_side.pipes.push_back(fd);
                } else {
                    unreachable!();
                }
            }

            for fd in disp_fds {
                if is_shm(&fd) {
                    disp_side.shm.push_back(fd);
                } else if is_pipe(&fd) {
                    disp_side.pipes.push_back(fd);
                } else {
                    unreachable!();
                }
            }

            last_disp_side = is_disp_side;
        }

        let side = if is_disp_side {
            &mut disp_side
        } else {
            &mut prog_side
        };

        match code {
            0 => {
                /* Queue following Wayland message */
                if data.len() < 8 {
                    /* Not enough space for header */
                    return;
                }
                let (obj, msg_len, _opcode) = parse_header(&data[..8]);
                if obj == DUMMY_OBJECT_ID {
                    // Prevent confusion with end message
                    return;
                }
                if msg_len % 4 != 0 || msg_len < 8 {
                    return;
                }
                let Some((msg, tail)) = data.split_at_checked(msg_len) else {
                    return;
                };
                data = tail;
                side.wayland_bytes.extend_from_slice(msg);
            }
            1 => {
                /* Rotate target file descriptor */
                if !side.pipes.is_empty() {
                    let fd = side.pipes.pop_front().unwrap();
                    side.pipes.push_back(fd);
                }
                if !side.shm.is_empty() {
                    let fd = side.shm.pop_front().unwrap();
                    side.shm.push_back(fd);
                }
            }
            2 => {
                /* Create and submit SHM buffer of size 0 */
                let fd = make_shm(0);
                side.wayland_fds.push(fd.try_clone().unwrap());
                side.shm.push_back(fd);
            }
            3 => {
                /* Create and submit SHM buffer of size 192 */
                let fd = make_shm(192);
                side.wayland_fds.push(fd.try_clone().unwrap());
                side.shm.push_back(fd);
            }
            4 => {
                /* Create and submit SHM buffer of size 10000 */
                let fd = make_shm(10000);
                side.wayland_fds.push(fd.try_clone().unwrap());
                side.shm.push_back(fd);
            }
            5 => {
                /* Create and submit SHM buffer of size 65536 */
                let fd = make_shm(65536);
                side.wayland_fds.push(fd.try_clone().unwrap());
                side.shm.push_back(fd);
            }
            6 => {
                /* Create and submit read end of pipe */
                let (pipe_r, pipe_w) = make_pipe();
                side.wayland_fds.push(pipe_r);
                side.pipes.push_back(pipe_w);
            }
            7 => {
                /* Create and submit write end of pipe */
                let (pipe_r, pipe_w) = make_pipe();
                side.wayland_fds.push(pipe_w);
                side.pipes.push_back(pipe_r);
            }
            8 => {
                /* Close a pipe end */
                drop(side.pipes.pop_front());
            }
            9 => {
                /* Try to read 100 bytes from a pipe */
                if let Some(fd) = side.pipes.front() {
                    let _ = unistd::read(fd, &mut scratch[..100]);
                }
            }
            10 => {
                /* Try to read 10000 bytes from a pipe */
                if let Some(fd) = side.pipes.front() {
                    let _ = unistd::read(fd, &mut scratch[..10000]);
                }
            }
            11 => {
                /* Try to write 100 bytes to a pipe */
                if let Some(fd) = side.pipes.front() {
                    let data = &mut scratch[..100];
                    for (i, d) in data.iter_mut().enumerate() {
                        *d = i as u8;
                    }
                    let _ = unistd::write(fd, data);
                }
            }
            12 => {
                /* Try to write 10000 bytes to a pipe */
                if let Some(fd) = side.pipes.front() {
                    let data = &mut scratch[..10000];
                    for (i, d) in data.iter_mut().enumerate() {
                        *d = (i as u8) ^ (((i * 111) >> 8) as u8) ^ (((i * 1111) >> 16) as u8);
                    }
                    let _ = unistd::write(fd, data);
                }
            }
            13 => {
                /* Modify first up to 10000 bytes of shm buffer */
                if let Some(fd) = side.shm.front() {
                    let file_len = get_shm_len(fd);

                    let tmp = &mut scratch[..10000.min(file_len)];

                    assert_eq!(uio::pread(fd, tmp, 0).unwrap(), tmp.len());
                    let mut x: u64 = 0;
                    for (i, d) in tmp.iter_mut().enumerate() {
                        x += (*d as u64) * (i as u64);
                        *d ^= ((x * 11111) >> 16) as u8;
                    }
                    assert_eq!(uio::pwrite(fd, tmp, 0).unwrap(), tmp.len());
                }
            }
            14 => {
                /* Modify last up to 10000 bytes of shm buffer */
                if let Some(fd) = side.shm.front() {
                    let file_len = get_shm_len(fd);

                    let tmp = &mut scratch[..10000.min(file_len)];
                    let offset = (file_len - tmp.len()) as i64;

                    assert_eq!(uio::pread(fd, tmp, offset).unwrap(), tmp.len());
                    let mut x: u64 = 0;
                    for (i, d) in tmp.iter_mut().enumerate() {
                        x += (*d as u64) * (i as u64);
                        *d ^= ((x * 11111) >> 16) as u8;
                    }
                    assert_eq!(uio::pwrite(fd, tmp, offset).unwrap(), tmp.len());
                }
            }
            // Other cases: do nothing
            _ => continue,
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let opts = Options {
        debug: false,

        compression: Compression::None, /* Avoid fuzzer traps, at the cost of missing possible bugs */
        video: VideoSetting {
            format: None,
            bits_per_frame: None,
            enc_pref: None,
            dec_pref: None,
        },
        threads: 1, /* minimum */
        title_prefix: "prefix-".to_string(),
        no_gpu: true,
        drm_node: None,
        debug_store_video: None,
        test_skip_vulkan: false,
        test_no_timeline_export: false,
        test_no_binary_semaphore_import: false,
    };

    if std::env::var_os("FUZZ_DEBUG").is_some() {
        log::set_max_level(LevelFilter::Trace);
        let _ = log::set_logger(&ThreadLogger);
    }

    let sigaction = signal::SigAction::new(
        signal::SigHandler::SigIgn,
        signal::SaFlags::empty(),
        signal::SigSet::empty(),
    );
    unsafe {
        // SAFETY: nothing should unsafely rely on SIGPIPE
        signal::sigaction(signal::Signal::SIGPIPE, &sigaction).unwrap();
    }

    /* Some systems set the soft limit rather low (to 1024 fds) */
    let (_lim_soft, lim_hard) = resource::getrlimit(resource::Resource::RLIMIT_NOFILE).unwrap();
    resource::setrlimit(resource::Resource::RLIMIT_NOFILE, lim_hard, lim_hard).unwrap();

    let (client_chan, server_chan) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )
    .expect("socket creation succeeds");
    let (client_prog, client_end) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )
    .expect("socket creation succeeds");
    let (server_prog, server_end) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )
    .expect("socket creation succeeds");

    let pollmask = signal::SigSet::empty();
    let stop_all = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let client_thread = thread::Builder::new()
            .name("waypipe-client".to_string())
            .spawn_scoped(scope, || {
                mainloop::main_interface_loop(
                    client_chan,
                    client_prog,
                    &opts,
                    WAYPIPE_PROTOCOL_VERSION,
                    true,
                    pollmask,
                    &stop_all,
                )
            })
            .expect("thread starting succeeds");

        let server_thread = thread::Builder::new()
            .name("waypipe-server".to_string())
            .spawn_scoped(scope, || {
                mainloop::main_interface_loop(
                    server_chan,
                    server_prog,
                    &opts,
                    MIN_PROTOCOL_VERSION,
                    false,
                    pollmask,
                    &stop_all,
                )
            })
            .expect("thread starting succeeds");

        run_endpoints(client_end, server_end, data);

        /* Once both endpoints disconnect, the main loop threads should cleanly exit */
        client_thread.join().unwrap().unwrap();
        server_thread.join().unwrap().unwrap();
    })
});
