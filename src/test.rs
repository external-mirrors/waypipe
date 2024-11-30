/* SPDX-License-Identifier: GPL-3.0-or-later */
#![cfg(test)]

#[cfg(feature = "dmabuf")]
use crate::dmabuf::*;
use crate::kernel::*;
use crate::mainloop::*;
use crate::util::*;
use crate::wayland::*;
use crate::wayland_gen::*;
use crate::*;
use nix::errno::Errno;
use nix::fcntl;
use nix::libc;
use nix::poll;
use nix::sys::signal;
use nix::sys::{memfd, socket, time};
use nix::unistd;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

struct TestLogger {
    max_level: log::LevelFilter,
    color_output: bool,
}

impl log::Log for TestLogger {
    fn enabled(&self, meta: &log::Metadata<'_>) -> bool {
        meta.level() <= self.max_level
    }
    fn log(&self, record: &log::Record<'_>) {
        if record.level() > self.max_level {
            return;
        }

        let time = SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        let t = if let Ok(t) = time {
            (t.as_nanos() % 100000000000u128) / 1000u128
        } else {
            0
        };
        let b = std::thread::current();
        let thread_name = b.name().unwrap_or("");
        let (esc1a, esc1b, esc1c) = if self.color_output {
            let c = if thread_name == "client" {
                "34"
            } else if thread_name == "server" {
                "36"
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
        let lvl_str: &str = match record.level() {
            log::Level::Error => "ERR",
            log::Level::Warn => "Wrn",
            log::Level::Debug => "dbg",
            log::Level::Info => "inf",
            log::Level::Trace => "trc",
        };

        let msg = format!(
            "{}{}{}[{:02}.{:06} {}({}) {}:{}]{} {}",
            esc1a,
            esc1b,
            esc1c,
            t / 1000000u128,
            t % 1000000u128,
            lvl_str,
            thread_name,
            record
                .file()
                .unwrap_or("src/unknown")
                .strip_prefix("src/")
                .unwrap(),
            record.line().unwrap_or(0),
            esc2,
            record.args(),
        );
        println!("{}", msg);
    }
    fn flush(&self) {
        /* not needed */
    }
}

/* Write messages to the Wayland connection, followed by a test message that should directly pass through;
 * possibly stopping early if the connection is closed.
 *
 * Returns: the messages received on the other end, minus the test message; if there was an error details
 * will be returned */
fn test_interact(
    prog: &OwnedFd,
    comp: &OwnedFd,
    write_to_prog: bool,
    data: &[u8],
    fds: &[&OwnedFd],
) -> (Vec<Vec<u8>>, Vec<OwnedFd>, Option<(ObjId, u32, String)>) {
    let mut end_msg = [0_u8; 8];
    let mut end_msg_view: &mut [u8] = &mut end_msg;
    write_header(&mut end_msg_view, ObjId(u32::MAX), 8, 0, 0);

    let mut nbytes_sent: usize = 0;
    let mut nfds_sent: usize = 0;
    let net_len = data.len() + end_msg.len();

    let raw_fds: Vec<i32> = fds.iter().map(|x| x.as_raw_fd()).collect();

    /* Not: libwayland sends up to 28 fds per byte */
    const MAX_FDS_PER_BYTE: usize = 32;
    assert!(data.len() >= (fds.len() + MAX_FDS_PER_BYTE - 1) / MAX_FDS_PER_BYTE);

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

    let start = Instant::now();
    let timeout = Duration::from_secs(1);
    let mut err: Option<(ObjId, u32, String)> = None;
    'outer: loop {
        let current = Instant::now();
        let elapsed = current.duration_since(start);
        if elapsed >= timeout {
            panic!("timeout: {:?}", elapsed);
        }
        let remaining = time::TimeSpec::from_duration(timeout.saturating_sub(elapsed));

        let mut pfds = Vec::new();
        let mut recvs: Vec<&mut ReadState> = Vec::new();
        let writing = nbytes_sent < net_len;
        if !recv_prog.eof {
            pfds.push(poll::PollFd::new(
                prog.as_fd(),
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
                comp.as_fd(),
                if writing && !write_to_prog {
                    poll::PollFlags::POLLIN | poll::PollFlags::POLLOUT
                } else {
                    poll::PollFlags::POLLIN
                },
            ));
            recvs.push(&mut recv_comp);
        }

        /* Connections should not close before either error or end message is received */
        assert!(!pfds.is_empty());

        let res = nix::poll::ppoll(&mut pfds, Some(remaining), None);
        if let Err(e) = res {
            assert!(e == Errno::EINTR || e == Errno::EAGAIN);
        }

        for (pfd, recv) in pfds.into_iter().zip(recvs.into_iter()) {
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
                    let (_obj_id, length, _opcode) = parse_wl_header(tail);
                    assert!(length >= 8 && length % 4 == 0);
                    if tail.len() >= length {
                        let (msg, nxt) = tail.split_at(length);
                        recv.rmsgs.push(msg.into());
                        tail = nxt;
                    }
                }
                recv.data.drain(..(recv.data.len() - tail.len()));
            } else if evt.contains(poll::PollFlags::POLLHUP) {
                recv.eof = true;
            } else if evt.contains(poll::PollFlags::POLLERR) {
                panic!("unexpected pollerr");
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
                    &iovs,
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
                    Err(e) => {
                        panic!("{:?}", e);
                    }
                }
            }
        }

        let opp_recv = if write_to_prog {
            &mut recv_comp
        } else {
            &mut recv_prog
        };
        for (i, msg) in opp_recv.rmsgs.iter().enumerate() {
            let (obj_id, _length, opcode) = parse_wl_header(msg);
            if obj_id == ObjId(u32::MAX) && opcode == 0 {
                /* encountered pass-through message; stop, are done */
                opp_recv.rmsgs.remove(i);
                break 'outer;
            }
        }

        for msg in &recv_prog.rmsgs {
            let (obj_id, _length, opcode) = parse_wl_header(msg);
            if obj_id == ObjId(1) && MethodId::Event(opcode) == OPCODE_WL_DISPLAY_ERROR {
                /* encountered error message; stop and return error */
                let (obj, code, msg) = parse_evt_wl_display_error(&msg).unwrap();
                err = Some((obj, code, String::from_utf8(Vec::from(msg)).unwrap()));
                break 'outer;
            }
        }
    }

    if write_to_prog {
        assert!(recv_prog.fds.is_empty());
        assert!((recv_prog.rmsgs.len() == (err.is_some() as usize)) && recv_prog.data.is_empty());
        (recv_comp.rmsgs, recv_comp.fds, err)
    } else {
        assert!(recv_comp.fds.is_empty());
        assert!(recv_comp.rmsgs.is_empty() && recv_comp.data.is_empty());
        (recv_prog.rmsgs, recv_prog.fds, err)
    }
}

/* Write messages to the Wayland connection, followed by a test message that should directly pass through;
 * possibly stopping early if the connection is closed. (Note: in that case, test_read_msgs should
 * capture an error message). */
fn test_write_msgs(socket: &OwnedFd, data: &[u8], fds: &[&OwnedFd]) {
    let mut end_msg = [0_u8; 8];
    let mut end_msg_view: &mut [u8] = &mut end_msg;
    write_header(&mut end_msg_view, ObjId(u32::MAX), 8, 0, 0);

    let mut nbytes_sent: usize = 0;
    let mut nfds_sent: usize = 0;
    let net_len = data.len() + end_msg.len();

    let raw_fds: Vec<i32> = fds.iter().map(|x| x.as_raw_fd()).collect();
    assert!(data.len() >= (fds.len() + 31) / 32);

    let start = Instant::now();
    let timeout = Duration::from_secs(1);

    while nbytes_sent < net_len {
        let iovs_long = [
            IoSlice::new(&data[std::cmp::min(nbytes_sent, data.len())..]),
            IoSlice::new(&end_msg[(std::cmp::max(nbytes_sent, data.len()) - data.len())..]),
        ];
        let iovs_short = [IoSlice::new(if nbytes_sent >= data.len() {
            &end_msg[nbytes_sent - data.len()..(nbytes_sent - data.len() + 1)]
        } else {
            &data[nbytes_sent..(nbytes_sent + 1)]
        })];
        let fds = &raw_fds[nfds_sent..std::cmp::min(fds.len(), nfds_sent + 32)];

        let short_transfer = fds.len() - nfds_sent >= 32;
        let iovs: &[IoSlice] = if short_transfer {
            &iovs_short
        } else {
            &iovs_long
        };
        let cmsgs = [socket::ControlMessage::ScmRights(fds)];

        let mut pfd = [poll::PollFd::new(socket.as_fd(), poll::PollFlags::POLLOUT)];
        let current = Instant::now();
        let elapsed = current.duration_since(start);
        if elapsed >= timeout {
            panic!("timeout: {:?}", elapsed);
        }
        let remaining = time::TimeSpec::from_duration(timeout.saturating_sub(elapsed));
        let res = nix::poll::ppoll(&mut pfd, Some(remaining), None);
        if let Err(e) = res {
            assert!(e == Errno::EINTR || e == Errno::EAGAIN);
        }
        let evts = pfd[0].revents().unwrap();
        if evts.contains(poll::PollFlags::POLLHUP) {
            println!("Pollhup on write");
            return;
        }
        if evts.contains(poll::PollFlags::POLLERR) {
            panic!("Unexpected pollerr");
        }
        if !evts.contains(poll::PollFlags::POLLOUT) {
            continue;
        }

        let r = nix::sys::socket::sendmsg::<()>(
            socket.as_raw_fd(),
            &iovs,
            if fds.is_empty() { &[] } else { &cmsgs },
            nix::sys::socket::MsgFlags::empty(),
            None,
        );
        match r {
            Ok(s) => {
                nbytes_sent += s;
                if short_transfer {
                    nfds_sent += 32;
                } else {
                    nfds_sent = fds.len();
                }
            }
            Err(Errno::EINTR) | Err(Errno::EAGAIN) => {
                println!("eintr");
            }
            Err(e) => {
                panic!("{:?}", e);
            }
        }
    }
}

struct ReadRecv {
    data: Vec<u8>,
    fds: Vec<OwnedFd>,
    eof: bool,
}

fn test_read_from_socket(socket: BorrowedFd, recv: &mut ReadRecv) -> bool {
    let mut tmp = vec![0u8; 16384];
    let mut iovs = [IoSliceMut::new(&mut tmp)];
    let mut cmsg_fds = nix::cmsg_space!([RawFd; 32]);

    let r = socket::recvmsg::<socket::UnixAddr>(
        socket.as_raw_fd(),
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
            nbytes == 0
        }
        Err(nix::errno::Errno::ECONNRESET) => true,
        Err(nix::errno::Errno::EINTR) | Err(nix::errno::Errno::EAGAIN) => false,
        Err(x) => {
            panic!("Error reading from socket: {:?}", x)
        }
    }
}

/* Read messages from the Wayland connection, until an error is returned or the test message is found.
 * If opposite_fd is not None, then it is the program-side socket and will receive wl_display_error. */
fn test_read_msgs(
    socket: &OwnedFd,
    opposite_socket: Option<&OwnedFd>,
) -> (Vec<Vec<u8>>, Vec<OwnedFd>, Option<(ObjId, u32, String)>) {
    let mut msgs = Vec::new();
    let mut fds = Vec::new();

    let mut pfds = Vec::new();
    let mut recv = Vec::new();
    pfds.push(poll::PollFd::new(socket.as_fd(), poll::PollFlags::POLLIN));
    recv.push(ReadRecv {
        data: Vec::new(),
        fds: Vec::new(),
        eof: false,
    });
    if let Some(f) = opposite_socket {
        pfds.push(poll::PollFd::new(f.as_fd(), poll::PollFlags::POLLIN));
        recv.push(ReadRecv {
            data: Vec::new(),
            fds: Vec::new(),
            eof: false,
        });
    }

    let start = Instant::now();
    let timeout = Duration::from_secs(1);
    let mut err: Option<(ObjId, u32, String)> = None;
    'outer: loop {
        let current = Instant::now();
        let elapsed = current.duration_since(start);
        if elapsed >= timeout {
            panic!("timeout: {:?}", elapsed);
        }
        let remaining = time::TimeSpec::from_duration(timeout.saturating_sub(elapsed));
        let res = nix::poll::ppoll(&mut pfds, Some(remaining), None);
        if let Err(e) = res {
            assert!(e == Errno::EINTR || e == Errno::EAGAIN);
        }

        for (p, r) in pfds.iter().zip(recv.iter_mut()) {
            let evt = p.revents().unwrap();

            if evt.contains(poll::PollFlags::POLLIN) {
                let eof = test_read_from_socket(p.as_fd(), r);
                if eof {
                    r.eof = true;
                }
            }
            if evt.contains(poll::PollFlags::POLLERR) {
                panic!("unexpected pollerr");
            }
        }

        if !recv[0].fds.is_empty() {
            fds.extend(recv[0].fds.drain(..));
        }

        while recv[0].data.len() >= 8 {
            let (obj_id, length, opcode) = parse_wl_header(&recv[0].data);
            assert!(length >= 8 && length % 4 == 0);

            if obj_id == ObjId(u32::MAX) && opcode == 0 {
                /* encountered pass-through message; we are done */
                break 'outer;
            }

            if recv[0].data.len() >= length {
                let data: Vec<u8> = recv[0].data.drain(..length).collect();

                if opposite_socket.is_none() {
                    if obj_id == ObjId(1) && MethodId::Event(opcode) == OPCODE_WL_DISPLAY_ERROR {
                        let (obj, code, msg) = parse_evt_wl_display_error(&data).unwrap();
                        err = Some((obj, code, String::from_utf8(Vec::from(msg)).unwrap()));
                        break 'outer;
                    }
                }

                msgs.push(data);
            }
        }

        if opposite_socket.is_some() {
            assert!(recv[1].fds.is_empty());
            if recv[1].data.len() >= 8 {
                let (obj_id, length, opcode) = parse_wl_header(&recv[1].data);
                if recv[1].data.len() >= length {
                    let data: Vec<u8> = recv[1].data.drain(..length).collect();
                    if obj_id == ObjId(1) && MethodId::Event(opcode) == OPCODE_WL_DISPLAY_ERROR {
                        let (obj, code, msg) = parse_evt_wl_display_error(&data).unwrap();
                        err = Some((obj, code, String::from_utf8(Vec::from(msg)).unwrap()));
                        break 'outer;
                    } else {
                        panic!("unexpected message on program side");
                    }
                }
            }
        }
    }

    (msgs, fds, err)
}

struct ProtocolTestContext {
    sock_prog: OwnedFd,
    sock_comp: OwnedFd,
}

impl ProtocolTestContext {
    fn prog_write(
        &mut self,
        data: &[u8],
        fds: &[&OwnedFd],
    ) -> Result<(Vec<Vec<u8>>, Vec<OwnedFd>), (ObjId, u32, String)> {
        let (msg, ofds, err) = test_interact(&self.sock_prog, &self.sock_comp, true, data, fds);
        if let Some(e) = err {
            Err(e)
        } else {
            Ok((msg, ofds))
        }
    }
    fn comp_write(
        &mut self,
        data: &[u8],
        fds: &[&OwnedFd],
    ) -> Result<(Vec<Vec<u8>>, Vec<OwnedFd>), (ObjId, u32, String)> {
        let (msg, ofds, err) = test_interact(&self.sock_prog, &self.sock_comp, false, data, fds);
        if let Some(e) = err {
            Err(e)
        } else {
            Ok((msg, ofds))
        }
    }

    fn prog_write_passthrough(&mut self, data: Vec<u8>) {
        assert!(is_plain_msgs(self.prog_write(&data, &[]), data));
    }
    fn comp_write_passthrough(&mut self, data: Vec<u8>) {
        assert!(is_plain_msgs(self.comp_write(&data, &[]), data));
    }
}

fn run_protocol_test_with_opts(
    test_fn: &dyn Fn(ProtocolTestContext) -> (),
    opts: &Options,
    wire_version: Option<u32>,
) {
    let max_level = log::LevelFilter::Debug;
    let logger = TestLogger {
        max_level,
        color_output: unistd::isatty(2).unwrap(),
    };
    log::set_max_level(max_level);
    // todo: this is global; any way to log per test? (E.g: making cargo test fork?)
    let _ = log::set_boxed_logger(Box::new(logger));

    let (channel1, channel2) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )
    .unwrap();
    let (prog_appl1, prog_appl2) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )
    .unwrap();
    let (prog_comp1, prog_comp2) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_NONBLOCK | socket::SockFlag::SOCK_CLOEXEC,
    )
    .unwrap();

    let opt_ref = opts;
    let wire_version = wire_version.unwrap_or(WAYPIPE_PROTOCOL_VERSION);
    let sigint_received = AtomicBool::new(false);
    let mut sigmask = signal::SigSet::empty();
    signal::pthread_sigmask(signal::SigmaskHow::SIG_SETMASK, None, Some(&mut sigmask)).unwrap();
    let sigrec_ref = &sigint_received;

    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("client".into())
            .spawn_scoped(scope, move || {
                main_interface_loop(
                    channel1,
                    prog_comp2,
                    opt_ref,
                    wire_version,
                    true,
                    sigmask,
                    sigrec_ref,
                )
            })
            .unwrap();

        std::thread::Builder::new()
            .name("server".into())
            .spawn_scoped(scope, move || {
                main_interface_loop(
                    channel2,
                    prog_appl2,
                    opt_ref,
                    MIN_PROTOCOL_VERSION,
                    false,
                    sigmask,
                    sigrec_ref,
                )
            })
            .unwrap();

        let ctx = ProtocolTestContext {
            sock_prog: prog_appl1,
            sock_comp: prog_comp1,
        };
        test_fn(ctx)
    });

    log::set_max_level(log::LevelFilter::Off);
}

fn run_protocol_test(test_fn: &dyn Fn(ProtocolTestContext) -> ()) {
    let options = Options {
        debug: true,
        compression: Compression::None,
        video: VideoSetting {
            format: None,
            bits_per_frame: None,
        },
        threads: 1,
        title_prefix: String::new(),
        no_gpu: false,
        drm_node: None,
        force_sw_encoding: false,
        force_sw_decoding: false,
    };
    run_protocol_test_with_opts(test_fn, &options, None);
}

fn build_msgs<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut &mut [u8]) -> (),
{
    let len = 16384;
    let mut buf = vec![0u8; len];
    let mut rest = &mut buf[..];
    f(&mut rest);
    let nwritten = len - rest.len();
    Vec::from(&buf[..nwritten])
}

fn is_plain_msgs(
    x: Result<(Vec<Vec<u8>>, Vec<OwnedFd>), (ObjId, u32, String)>,
    concat: Vec<u8>,
) -> bool {
    if let Ok((msg, fds)) = x {
        fds.is_empty() && msg.concat() == concat
    } else {
        false
    }
}

#[test]
fn proto_basic() {
    /* Test to verify that a simple message exchange behaves as expected */
    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let write_prog = build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, ObjId(1), ObjId(2));
            write_req_wl_display_sync(dst, ObjId(1), ObjId(3));
        });
        let (resp, resp_fds) = ctx.prog_write(&write_prog, &[]).unwrap();
        assert!(resp_fds.is_empty());
        assert!(resp.concat() == write_prog);

        let write_comp = build_msgs(|dst| {
            write_evt_wl_registry_global(dst, ObjId(2), 1, "wl_compositor".as_bytes(), 3);
            write_evt_wl_callback_done(dst, ObjId(3), 0);
        });
        assert!(is_plain_msgs(ctx.comp_write(&write_comp, &[]), write_comp));
    });
}

#[test]
fn proto_base_wire() {
    /* Test that using the base protocol version still works, for basic operations */
    let opts = Options {
        debug: false,
        compression: Compression::None,
        video: VideoSetting {
            format: None,
            bits_per_frame: None,
        },
        threads: 1,
        title_prefix: String::new(),
        no_gpu: true,
        drm_node: None,
        force_sw_encoding: false,
        force_sw_decoding: false,
    };
    run_protocol_test_with_opts(
        &|mut ctx: ProtocolTestContext| {
            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_display_get_registry(dst, ObjId(1), ObjId(2));
                write_req_wl_display_sync(dst, ObjId(1), ObjId(3));
            }));
            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_callback_done(dst, ObjId(3), 0);
            }));
            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_display_sync(dst, ObjId(1), ObjId(3));
            }));
            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_callback_done(dst, ObjId(3), 0);
            }));
        },
        &opts,
        Some(MIN_PROTOCOL_VERSION),
    );
}

fn make_file_with_contents(data: &[u8]) -> Result<OwnedFd, String> {
    let local_fd = memfd::memfd_create(
        c"/waypipe",
        memfd::MemFdCreateFlag::MFD_CLOEXEC | memfd::MemFdCreateFlag::MFD_ALLOW_SEALING,
    )
    .map_err(|x| tag!("Failed to create memfd: {:?}", x))?;
    unistd::ftruncate(&local_fd, data.len().try_into().unwrap())
        .map_err(|x| tag!("Failed to resize memfd: {:?}", x))?;

    let mapping = ExternalMapping::new(&local_fd, data.len(), false)?;
    copy_onto_mapping(data, &mapping, 0);

    Ok(local_fd)
}
fn update_file_contents(fd: &OwnedFd, data: &[u8]) -> Result<(), String> {
    let mapping = ExternalMapping::new(fd, data.len(), false)?;
    copy_onto_mapping(data, &mapping, 0);
    drop(mapping);
    Ok(())
}
fn resize_file_with_contents(fd: &OwnedFd, data: &[u8]) -> Result<(), String> {
    unistd::ftruncate(&fd, data.len().try_into().unwrap())
        .map_err(|x| tag!("Failed to resize memfd: {:?}", x))?;
    let mapping = ExternalMapping::new(&fd, data.len(), false)?;
    copy_onto_mapping(data, &mapping, 0);
    Ok(())
}
fn get_file_contents(fd: &OwnedFd, len: usize) -> Result<Vec<u8>, String> {
    let mapping = ExternalMapping::new(fd, len, true)?;
    let mut data = vec![0xff_u8; len];
    copy_from_mapping(&mut data, &mapping, 0);
    drop(mapping);
    Ok(data)
}

#[test]
fn proto_keymap() {
    /* Test to verify that keymap files can be transferred reliably */
    for length in [0, 9, 4096, 300001] {
        run_protocol_test(&|mut ctx: ProtocolTestContext| {
            let source_text = "test data ".as_bytes();
            let mut source_data =
                source_text.repeat(align(length, source_text.len()) / source_text.len());
            source_data.truncate(length);
            let source_fd = make_file_with_contents(&source_data).unwrap();

            let (display, registry, callback, seat, keyboard) =
                (ObjId(1), ObjId(2), ObjId(3), ObjId(4), ObjId(5));

            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_display_get_registry(dst, display, registry);
                write_req_wl_display_sync(dst, display, callback);
            }));

            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_registry_global(dst, registry, 1, "wl_seat".as_bytes(), 7);
                write_evt_wl_callback_done(dst, callback, 0);
            }));

            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_registry_bind(dst, registry, 1, "wl_seat".as_bytes(), 7, seat);
            }));

            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_seat_capabilities(dst, seat, 3);
            }));

            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_seat_get_keyboard(dst, seat, keyboard);
            }));

            let (mut msgs, mut fds) = ctx
                .comp_write(
                    &build_msgs(|dst| {
                        write_evt_wl_keyboard_keymap(
                            dst,
                            keyboard,
                            true,
                            1,
                            source_data.len() as _,
                        );
                    }),
                    &[&source_fd],
                )
                .unwrap();
            drop(source_fd);

            let (_format, keymap_length) = parse_evt_wl_keyboard_keymap(&mut msgs[0]).unwrap();

            let new_kb_fd = fds.remove(0);
            let new_data = get_file_contents(&new_kb_fd, keymap_length as usize).unwrap();
            assert!(new_data == source_data);
        });
    }
}

/* Send `data` into `src` and check that it comes out of `dst` */
fn check_pipe_transfer(pipe_w: OwnedFd, pipe_r: OwnedFd, data: &[u8]) {
    let mut nwritten = 0;
    let start = Instant::now();
    let timeout = Duration::from_secs(1);

    let mut ord = Some(pipe_r);
    let mut owr = Some(pipe_w);
    if data.len() == 0 {
        owr = None;
    }

    let mut recv = Vec::new();

    let mut tmp = vec![0; 4096];
    while ord.is_some() || owr.is_some() {
        let mut pfds = Vec::new();
        let wr_idx = owr.as_ref().and_then(|x| {
            pfds.push(poll::PollFd::new(x.as_fd(), poll::PollFlags::POLLOUT));
            Some(pfds.len() - 1)
        });
        let rd_idx = ord.as_ref().and_then(|x| {
            pfds.push(poll::PollFd::new(x.as_fd(), poll::PollFlags::POLLIN));
            Some(pfds.len() - 1)
        });
        let current = Instant::now();
        let elapsed = current.duration_since(start);
        if elapsed >= timeout {
            panic!("timeout: {:?}", elapsed);
        }
        let remaining = time::TimeSpec::from_duration(timeout.saturating_sub(elapsed));
        let ret = poll::ppoll(&mut pfds, Some(remaining), None);
        if let Err(e) = ret {
            assert!(e == Errno::EINTR);
        }
        let rev_wr = wr_idx.and_then(|i| Some(pfds[i].revents().unwrap()));
        let rev_rd = rd_idx.and_then(|i| Some(pfds[i].revents().unwrap()));

        if let Some(evts) = rev_rd {
            assert!(!evts.contains(poll::PollFlags::POLLERR));

            if evts.contains(poll::PollFlags::POLLIN) {
                match unistd::read(ord.as_ref().unwrap().as_raw_fd(), &mut tmp) {
                    Err(Errno::EINTR) | Err(Errno::EAGAIN) => { /* do nothing */ }
                    Err(Errno::ECONNRESET) | Err(Errno::ENOTCONN) => {
                        ord = None;
                    }
                    Err(x) => panic!("{:?}", x),
                    Ok(len) => {
                        if len > 0 {
                            recv.extend_from_slice(&tmp[..len]);
                        } else {
                            /* nothing more to read */
                            ord = None;
                        }
                    }
                }
            } else if evts.contains(poll::PollFlags::POLLHUP) {
                /* case: hangup, no pending data */
                ord = None;
            }
        }
        if let Some(evts) = rev_wr {
            assert!(!evts.contains(poll::PollFlags::POLLERR));
            if evts.contains(poll::PollFlags::POLLHUP) {
                owr = None;
            } else if evts.contains(poll::PollFlags::POLLOUT) {
                match unistd::write(owr.as_ref().unwrap(), &data[nwritten..]) {
                    Err(Errno::EINTR) | Err(Errno::EAGAIN) => { /* do nothing */ }
                    Err(Errno::EPIPE) | Err(Errno::ECONNRESET) => {
                        owr = None;
                    }
                    Err(x) => panic!("{:?}", x),
                    Ok(len) => {
                        nwritten += len;
                        if nwritten == data.len() {
                            owr = None;
                        }
                    }
                }
            }
        }
    }

    assert!(recv == data);
}

fn get_intf_name(intf: WaylandInterface) -> &'static [u8] {
    INTERFACE_TABLE[intf as usize].name.as_bytes()
}

#[test]
fn proto_pipe_write() {
    let (display, registry, manager, seat, dev, source) =
        (ObjId(1), ObjId(2), ObjId(3), ObjId(4), ObjId(5), ObjId(6));
    let seat_name = get_intf_name(WaylandInterface::WlSeat);
    let ddev_name = get_intf_name(WaylandInterface::WlDataDeviceManager);
    let prim_name = get_intf_name(WaylandInterface::ZwpPrimarySelectionDeviceManagerV1);
    let data_name = get_intf_name(WaylandInterface::ExtDataControlManagerV1);
    let gtk_name = get_intf_name(WaylandInterface::GtkPrimarySelectionDeviceManager);
    let wlr_name = get_intf_name(WaylandInterface::ZwlrDataControlManagerV1);
    let mime = "text/plain;charset=utf-8".as_bytes();

    /* Protocol sequences leading to a pipe receipt; in all cases the pipe is provided with the last message */
    let ex_wl: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, ddev_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, ddev_name, 1, manager);
            write_req_wl_data_device_manager_get_data_device(dst, manager, dev, seat);
            write_req_wl_data_device_manager_create_data_source(dst, manager, source);
            write_req_wl_data_source_offer(dst, source, mime);
            write_req_wl_data_device_set_selection(dst, dev, source, 99);
        }),
        build_msgs(|dst| {
            write_evt_wl_data_source_send(dst, source, true, mime);
        }),
    ];
    let ex_prim: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, prim_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, prim_name, 1, manager);
            write_req_zwp_primary_selection_device_manager_v1_get_device(dst, manager, dev, seat);
            write_req_zwp_primary_selection_device_manager_v1_create_source(dst, manager, source);
            write_req_zwp_primary_selection_source_v1_offer(dst, source, mime);
            write_req_zwp_primary_selection_device_v1_set_selection(dst, dev, source, 99);
        }),
        build_msgs(|dst| {
            write_evt_zwp_primary_selection_source_v1_send(dst, source, true, mime);
        }),
    ];
    let ex_data: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, data_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, data_name, 1, manager);
            write_req_ext_data_control_manager_v1_get_data_device(dst, manager, dev, seat);
            write_req_ext_data_control_manager_v1_create_data_source(dst, manager, source);
            write_req_ext_data_control_source_v1_offer(dst, source, mime);
            write_req_ext_data_control_device_v1_set_selection(dst, dev, source);
        }),
        build_msgs(|dst| {
            write_evt_ext_data_control_source_v1_send(dst, source, true, mime);
        }),
    ];
    let ex_gtk: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, gtk_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, gtk_name, 1, manager);
            write_req_gtk_primary_selection_device_manager_get_device(dst, manager, dev, seat);
            write_req_gtk_primary_selection_device_manager_create_source(dst, manager, source);
            write_req_gtk_primary_selection_source_offer(dst, source, mime);
            write_req_gtk_primary_selection_device_set_selection(dst, dev, source, 99);
        }),
        build_msgs(|dst| {
            write_evt_gtk_primary_selection_source_send(dst, source, true, mime);
        }),
    ];
    let ex_wlr: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, &wlr_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, wlr_name, 1, manager);
            write_req_zwlr_data_control_manager_v1_get_data_device(dst, manager, dev, seat);
            write_req_zwlr_data_control_manager_v1_create_data_source(dst, manager, source);
            write_req_zwlr_data_control_source_v1_offer(dst, source, mime);
            write_req_zwlr_data_control_device_v1_set_selection(dst, dev, source);
        }),
        build_msgs(|dst| {
            write_evt_zwlr_data_control_source_v1_send(dst, source, true, mime);
        }),
    ];

    let (display, registry, manager, seat, dev, offer) =
        (ObjId(1), ObjId(2), ObjId(3), ObjId(4), ObjId(5), ObjId(6));
    /* Protocol sequences leading to a pipe receipt; in all cases the pipe is provided with the last message */
    let ex2_wl: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, ddev_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, ddev_name, 1, manager);
            write_req_wl_data_device_manager_get_data_device(dst, manager, dev, seat);
        }),
        build_msgs(|dst| {
            write_evt_wl_data_device_data_offer(dst, dev, offer);
            write_evt_wl_data_offer_offer(dst, dev, mime);
            write_evt_wl_data_device_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_wl_data_offer_receive(dst, offer, true, mime);
        }),
    ];
    let ex2_prim: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, prim_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, prim_name, 1, manager);
            write_req_zwp_primary_selection_device_manager_v1_get_device(dst, manager, dev, seat);
        }),
        build_msgs(|dst| {
            write_evt_zwp_primary_selection_device_v1_data_offer(dst, dev, offer);
            write_evt_zwp_primary_selection_offer_v1_offer(dst, dev, mime);
            write_evt_zwp_primary_selection_device_v1_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_zwp_primary_selection_offer_v1_receive(dst, offer, true, mime);
        }),
    ];
    let ex2_data: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, data_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, data_name, 1, manager);
            write_req_ext_data_control_manager_v1_get_data_device(dst, manager, dev, seat);
        }),
        build_msgs(|dst| {
            write_evt_ext_data_control_device_v1_data_offer(dst, dev, offer);
            write_evt_ext_data_control_offer_v1_offer(dst, dev, mime);
            write_evt_ext_data_control_device_v1_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_ext_data_control_offer_v1_receive(dst, offer, true, mime);
        }),
    ];
    let ex2_gtk: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, gtk_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, gtk_name, 1, manager);
            write_req_gtk_primary_selection_device_manager_get_device(dst, manager, dev, seat);
        }),
        build_msgs(|dst| {
            write_evt_gtk_primary_selection_device_data_offer(dst, dev, offer);
            write_evt_gtk_primary_selection_offer_offer(dst, dev, mime);
            write_evt_gtk_primary_selection_device_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_gtk_primary_selection_offer_receive(dst, offer, true, mime);
        }),
    ];
    let ex2_wlr: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, &wlr_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, wlr_name, 1, manager);
            write_req_zwlr_data_control_manager_v1_get_data_device(dst, manager, dev, seat);
        }),
        build_msgs(|dst| {
            write_evt_zwlr_data_control_device_v1_data_offer(dst, dev, offer);
            write_evt_zwlr_data_control_offer_v1_offer(dst, dev, mime);
            write_evt_zwlr_data_control_device_v1_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_zwlr_data_control_offer_v1_receive(dst, offer, true, mime);
        }),
    ];

    let test_cases: &[&[Vec<u8>]] = &[
        ex_wl, ex_prim, ex_data, ex_gtk, ex_wlr, ex2_wl, ex2_prim, ex2_data, ex2_gtk, ex2_wlr,
    ];

    let lengths = [100_usize, 0_usize, 131073_usize]
        .iter()
        .chain(std::iter::repeat(&256_usize));
    for (test, length) in test_cases.iter().zip(lengths) {
        run_protocol_test(&|mut ctx: ProtocolTestContext| {
            let (pipe_r, pipe_w) =
                unistd::pipe2(fcntl::OFlag::O_CLOEXEC | fcntl::OFlag::O_NONBLOCK).unwrap();
            for (i, line) in test.iter().enumerate().take(test.iter().len() - 1) {
                if i % 2 == 0 {
                    ctx.prog_write_passthrough(line.clone());
                } else {
                    ctx.comp_write_passthrough(line.clone());
                }
            }

            let end = test.last().unwrap();
            let ifds = [&pipe_w];
            let (msg, mut ofds) = if test.iter().len() % 2 == 0 {
                ctx.comp_write(end, &ifds)
            } else {
                ctx.prog_write(end, &ifds)
            }
            .unwrap();
            assert!(msg.concat() == *end);
            drop(pipe_w);
            let pipe_w = ofds.remove(0);

            let mut data = "test".as_bytes().repeat(align(*length, 4) / 4);
            data.truncate(*length);

            check_pipe_transfer(pipe_w, pipe_r, &data);
        });
    }
}

#[test]
fn proto_presentation_time() {
    /* Test to verify that presentation time handling does not introduce major errors */
    for (pres_clock, fast_start) in [
        (libc::CLOCK_MONOTONIC as u32, true),
        (libc::CLOCK_REALTIME as u32, false),
    ] {
        run_protocol_test(&|mut ctx: ProtocolTestContext| {
            let (display, registry, pres, comp, surface, feedback) =
                (ObjId(1), ObjId(2), ObjId(3), ObjId(4), ObjId(5), ObjId(6));

            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_display_get_registry(dst, display, registry);
            }));

            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_registry_global(dst, registry, 1, "wp_presentation".as_bytes(), 1);
                write_evt_wl_registry_global(dst, registry, 2, "wl_compositor".as_bytes(), 1);
            }));

            let start = Instant::now();

            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_registry_bind(dst, registry, 1, "wp_presentation".as_bytes(), 1, pres);
                write_req_wl_registry_bind(dst, registry, 2, "wl_compositor".as_bytes(), 1, comp);
                write_req_wl_compositor_create_surface(dst, comp, surface);
                write_req_wl_surface_damage(dst, surface, 0, 0, 64, 64);
                if fast_start {
                    write_req_wp_presentation_feedback(dst, pres, surface, feedback);
                    write_req_wp_presentation_destroy(dst, pres);
                    write_req_wl_surface_commit(dst, surface);
                }
            }));

            if !fast_start {
                ctx.comp_write_passthrough(build_msgs(|dst| {
                    write_evt_wp_presentation_clock_id(dst, pres, pres_clock);
                }));
                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wp_presentation_feedback(dst, pres, surface, feedback);
                }));
            }

            let init_time_ns = 1111111111111u128;
            let data = build_msgs(|dst| {
                if fast_start {
                    write_evt_wp_presentation_clock_id(dst, pres, pres_clock);
                    write_evt_wl_display_delete_id(dst, display, pres.0);
                }
                write_evt_wp_presentation_feedback_presented(
                    dst,
                    feedback,
                    0,
                    (init_time_ns / 1000000000) as u32,
                    (init_time_ns % 1000000000) as u32,
                    500000000,
                    0,
                    10,
                    0x9,
                );
            });
            let (msgs, fds) = ctx.comp_write(&data, &[]).unwrap();
            let end = Instant::now();
            assert!(fds.is_empty());
            if fast_start {
                assert!(
                    msgs[..2].concat()
                        == data[..length_evt_wp_presentation_clock_id()
                            + length_evt_wl_display_delete_id()]
                );
            }
            assert!(
                parse_wl_header(&msgs.last().unwrap())
                    == (
                        feedback,
                        length_evt_wp_presentation_feedback_presented(),
                        OPCODE_WP_PRESENTATION_FEEDBACK_PRESENTED.code()
                    )
            );
            let (tv_sec_hi, tv_sec_lo, tv_nsec, _refresh, _seq_hi, _seq_lo, _flags) =
                parse_evt_wp_presentation_feedback_presented(&msgs.last().unwrap()).unwrap();
            let output_ns =
                1000000000 * (join_u64(tv_sec_hi, tv_sec_lo) as u128) + (tv_nsec as u128);

            /* The time adjustment uses two XYX measurements, whose absolute error
             * is ≤ half the elapsed time each, assuming the clocks run at the same
             * rate and do not change. */
            let max_time_error = end.duration_since(start).saturating_mul(2);

            let abs_diff = output_ns.abs_diff(init_time_ns);
            println!(
                "clock {}: roundtrip diff: {} ns, max permissible error: {} ns",
                pres_clock,
                (abs_diff as i128) * (if output_ns > init_time_ns { 1 } else { -1 }),
                max_time_error.as_nanos()
            );

            assert!(abs_diff < max_time_error.as_nanos());
        });
    }
}

#[test]
fn proto_object_collision() {
    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (display, registry) = (ObjId(1), ObjId(2));
        let msgs = build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
            write_req_wl_display_get_registry(dst, display, registry);
        });
        let res = ctx.prog_write(&msgs, &[]);
        if let Err(ref e) = res {
            println!("error: {:?}", e);
        }
        assert!(res.is_err());
    });
}

#[test]
fn proto_shm_buffer() {
    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (display, registry, shm, comp, surface, pool, buffer) = (
            ObjId(1),
            ObjId(2),
            ObjId(3),
            ObjId(4),
            ObjId(5),
            ObjId(6),
            ObjId(7),
        );

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }));

        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, "wl_shm".as_bytes(), 2);
            write_evt_wl_registry_global(dst, registry, 2, "wl_compositor".as_bytes(), 6);
        }));

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, "wl_shm".as_bytes(), 2, shm);
            write_req_wl_registry_bind(dst, registry, 2, "wl_compositor".as_bytes(), 6, comp);
            write_req_wl_compositor_create_surface(dst, comp, surface);
        }));

        /* First, simple test case: an empty shm pool, never modified */
        let empty_fd = make_file_with_contents(&[]).unwrap();
        let msg = build_msgs(|dst| {
            write_req_wl_shm_create_pool(dst, shm, true, pool, 0);
        });
        let (rmsg, mut rfd) = ctx.prog_write(&msg[..], &[&empty_fd]).unwrap();
        assert!(rmsg.concat() == msg);
        drop(empty_fd);
        assert!(rfd.len() == 1);
        let output_fd = rfd.remove(0);
        assert!(get_file_contents(&output_fd, 0).unwrap().is_empty());

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_shm_pool_destroy(dst, pool);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_display_delete_id(dst, display, pool.0);
        }));

        /* pools and images in various sizes */
        for (w, h) in [(3, 3), (16, 16), (1023, 1025)] {
            let sz: usize = w * h;
            let mut data = vec![0; sz];
            let mut i: u8 = 0x80;
            /* Draw a square inside */
            for y in (h / 3)..(2 * h) / 3 {
                for x in (w / 3)..(2 * w) / 3 {
                    data[y * w + x] = i;
                    i = i.wrapping_add(1);
                }
            }
            let buf_fd = make_file_with_contents(&data[..]).unwrap();
            let msgs = build_msgs(|dst| {
                write_req_wl_shm_create_pool(dst, shm, true, pool, sz as i32);
                write_req_wl_shm_pool_create_buffer(
                    dst,
                    pool,
                    buffer,
                    0,
                    w as i32,
                    h as i32,
                    w as i32,
                    WlShmFormat::R8 as u32,
                );
                write_req_wl_surface_attach(dst, surface, buffer, 0, 0);
                write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                write_req_wl_surface_commit(dst, surface);
            });
            let (rmsg, mut rfd) = ctx.prog_write(&msgs[..], &[&buf_fd]).unwrap();
            assert!(rmsg.concat() == msgs);
            drop(buf_fd);
            assert!(rfd.len() == 1);
            let output_fd = rfd.remove(0);
            let output = get_file_contents(&output_fd, sz).unwrap();
            assert!(output == data);

            /* Cleanup */
            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_shm_pool_destroy(dst, pool);
                write_req_wl_buffer_destroy(dst, buffer);
            }));
            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_display_delete_id(dst, display, pool.0);
                write_evt_wl_display_delete_id(dst, display, buffer.0);
            }));
        }
    });
}

#[test]
fn proto_shm_extend() {
    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (display, registry, shm, comp, surface, pool, buf_a, buf_b, buf_c) = (
            ObjId(1),
            ObjId(2),
            ObjId(3),
            ObjId(4),
            ObjId(5),
            ObjId(6),
            ObjId(7),
            ObjId(8),
            ObjId(9),
        );

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }));

        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, "wl_shm".as_bytes(), 2);
            write_evt_wl_registry_global(dst, registry, 2, "wl_compositor".as_bytes(), 6);
        }));

        let pool_fd = make_file_with_contents(&[]).unwrap();

        let msg = build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, "wl_shm".as_bytes(), 2, shm);
            write_req_wl_registry_bind(dst, registry, 2, "wl_compositor".as_bytes(), 6, comp);
            write_req_wl_compositor_create_surface(dst, comp, surface);
            write_req_wl_shm_create_pool(dst, shm, true, pool, 0);
        });
        let (rmsg, mut rfd) = ctx.prog_write(&msg[..], &[&pool_fd]).unwrap();
        assert!(rmsg.concat() == msg);
        assert!(rfd.len() == 1);
        let mirror_fd = rfd.remove(0);
        assert!(get_file_contents(&mirror_fd, 0).unwrap().is_empty());

        let (w, h): (usize, usize) = (15, 15);
        let fmt = WlShmFormat::Abgr16161616;

        let buf_ids = [buf_a, buf_b, buf_c];
        let mut i: u16 = 0x1000;
        for nblocks in 1..=3 {
            let new_sz = w * h * 8 * nblocks;
            let mut img_data = vec![0u8; new_sz];
            for k in 0..nblocks {
                /* Fill with gradient */
                let block_data = &mut img_data[(w * h * 8 * k)..(w * h * 8 * (k + 1))];
                for j in 0..(w * h) {
                    for h in 0..4 {
                        block_data[(8 * j + 2 * h)..(8 * j + 2 * h + 2)]
                            .copy_from_slice(&((h as u16 + 1) * i).to_le_bytes());
                    }
                    i = i.wrapping_add(1);
                }
            }

            resize_file_with_contents(&pool_fd, &img_data).unwrap();

            ctx.prog_write_passthrough(build_msgs(|dst| {
                for i in 0..nblocks {
                    if i == nblocks - 1 {
                        write_req_wl_shm_pool_resize(dst, pool, new_sz as i32);
                        write_req_wl_shm_pool_create_buffer(
                            dst,
                            pool,
                            buf_ids[nblocks - 1],
                            (w * h * 8 * (nblocks - 1)) as i32,
                            w as i32,
                            h as i32,
                            (w * 8) as i32,
                            fmt as u32,
                        );
                    }

                    write_req_wl_surface_attach(dst, surface, buf_ids[i], 0, 0);
                    write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                    write_req_wl_surface_commit(dst, surface);
                }
            }));

            let mir = get_file_contents(&mirror_fd, new_sz).unwrap();
            assert!(mir == img_data);
        }
    });
}

#[cfg(feature = "dmabuf")]
fn setup_linux_dmabuf(
    ctx: &mut ProtocolTestContext,
    vulk: &Vulkan,
    display: ObjId,
    registry: ObjId,
    dmabuf: ObjId,
    comp: ObjId,
    surface: ObjId,
    feedback: ObjId,
) {
    ctx.prog_write_passthrough(build_msgs(|dst| {
        write_req_wl_display_get_registry(dst, display, registry);
    }));

    ctx.comp_write_passthrough(build_msgs(|dst| {
        write_evt_wl_registry_global(dst, registry, 1, "zwp_linux_dmabuf_v1".as_bytes(), 5);
        write_evt_wl_registry_global(dst, registry, 2, "wl_compositor".as_bytes(), 6);
    }));

    ctx.prog_write_passthrough(build_msgs(|dst| {
        write_req_wl_registry_bind(
            dst,
            registry,
            1,
            "zwp_linux_dmabuf_v1".as_bytes(),
            5,
            dmabuf,
        );
        write_req_wl_registry_bind(dst, registry, 2, "wl_compositor".as_bytes(), 6, comp);
        write_req_wl_compositor_create_surface(dst, comp, surface);
        write_req_zwp_linux_dmabuf_v1_get_default_feedback(dst, dmabuf, feedback);
    }));

    let main_device = vulk.get_device();
    let advertised_formats = [
        wayland_to_drm(WlShmFormat::Rgb565),
        wayland_to_drm(WlShmFormat::Xrgb8888),
        wayland_to_drm(WlShmFormat::Xrgb16161616),
    ];
    let mut table: Vec<u8> = Vec::new();
    let mut array: Vec<u8> = Vec::new();
    let mut i: u16 = 0;
    for fmt in advertised_formats {
        let modifier_list = vulk.get_supported_modifiers(fmt);
        for m in modifier_list {
            table.extend_from_slice(&fmt.to_le_bytes());
            table.extend_from_slice(&0_u32.to_le_bytes());
            table.extend_from_slice(&m.to_le_bytes());
            array.extend_from_slice(&i.to_le_bytes());
            i += 1;
        }
    }
    let table_fd = make_file_with_contents(&table).unwrap();
    let msgs = build_msgs(|dst| {
        write_evt_zwp_linux_dmabuf_feedback_v1_format_table(
            dst,
            feedback,
            true,
            table.len() as u32,
        );
        write_evt_zwp_linux_dmabuf_feedback_v1_main_device(
            dst,
            feedback,
            &main_device.to_le_bytes(),
        );
        write_evt_zwp_linux_dmabuf_feedback_v1_tranche_target_device(
            dst,
            feedback,
            &main_device.to_le_bytes(),
        );
        write_evt_zwp_linux_dmabuf_feedback_v1_tranche_flags(dst, feedback, 0);
        write_evt_zwp_linux_dmabuf_feedback_v1_tranche_formats(dst, feedback, &array[..]);
        write_evt_zwp_linux_dmabuf_feedback_v1_tranche_done(dst, feedback);
        write_evt_zwp_linux_dmabuf_feedback_v1_done(dst, feedback);
    });
    let (_rmsgs, rfds) = ctx.comp_write(&msgs[..], &[&table_fd]).unwrap();
    assert!(rfds.len() == 1);
    drop(rfds);
    /* exact replication not guaranteed for these; parsing returned messages is not critical since support _should_ be present */
}

#[cfg(feature = "dmabuf")]
#[test]
fn proto_dmabuf() {
    let vulk = setup_vulkan(None, false, true, false, false).unwrap();

    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (display, registry, dmabuf, comp, surface, feedback, params, buffer, sbuffer) = (
            ObjId(1),
            ObjId(2),
            ObjId(3),
            ObjId(4),
            ObjId(5),
            ObjId(6),
            ObjId(7),
            ObjId(8),
            ObjId(0xff000000),
        );

        setup_linux_dmabuf(
            &mut ctx, &vulk, display, registry, dmabuf, comp, surface, feedback,
        );

        for (w, h, immed) in [(3, 4, true), (64, 64, true), (513, 511, false)] {
            let fmt = wayland_to_drm(WlShmFormat::Rgb565);
            let bpp = 2;

            let mut img_data = vec![0u8; (w * h) as usize * bpp];
            let mut i: u16 = 0x1234;
            /* Draw a square inside */
            for y in (h / 3)..(2 * h) / 3 {
                for x in (w / 3)..(2 * w) / 3 {
                    img_data[2 * (y * w + x)..2 * (y * w + x) + 2]
                        .copy_from_slice(&i.to_le_bytes());
                    i = i.wrapping_add(1);
                }
            }

            let modifier_list = vulk.get_supported_modifiers(fmt);
            let (img, planes) =
                vulkan_create_dmabuf(&vulk, w as u32, h as u32, fmt, &modifier_list, false)
                    .unwrap();

            let msgs = build_msgs(|dst| {
                write_req_zwp_linux_dmabuf_v1_create_params(dst, dmabuf, params);
                for p in planes.iter() {
                    let (mod_hi, mod_lo) = split_u64(p.modifier);
                    write_req_zwp_linux_buffer_params_v1_add(
                        dst,
                        params,
                        true,
                        p.plane_idx,
                        p.offset,
                        p.stride,
                        mod_hi,
                        mod_lo,
                    );
                }

                if immed {
                    write_req_zwp_linux_buffer_params_v1_create_immed(
                        dst, params, buffer, w as i32, h as i32, fmt, 0,
                    );
                } else {
                    write_req_zwp_linux_buffer_params_v1_create(
                        dst, params, w as i32, h as i32, fmt, 0,
                    );
                }
            });
            let plane_fds: Vec<&OwnedFd> = planes.iter().map(|p| &p.fd).collect();
            let (rmsgs, mut rfds) = ctx.prog_write(&msgs[..], &plane_fds[..]).unwrap();
            drop(planes);
            let add_msgs = &rmsgs[1..rmsgs.len() - 1];
            assert!(rfds.len() == add_msgs.len());
            let create_msg = &rmsgs[rmsgs.len() - 1];
            let (rw, rh, rfmt) = if immed {
                let (_rbuf, rw, rh, rfmt, _rflags) =
                    parse_req_zwp_linux_buffer_params_v1_create_immed(&create_msg[..]).unwrap();
                (rw, rh, rfmt)
            } else {
                let (rw, rh, rfmt, _rflags) =
                    parse_req_zwp_linux_buffer_params_v1_create(&create_msg[..]).unwrap();
                (rw, rh, rfmt)
            };
            assert!((rw, rh, rfmt) == (w as i32, h as i32, fmt));

            let mut planes = Vec::new();
            for (fd, msg) in rfds.drain(..).zip(add_msgs.iter()) {
                let (plane_idx, offset, stride, mod_hi, mod_lo) =
                    parse_req_zwp_linux_buffer_params_v1_add(msg).unwrap();
                let modifier = join_u64(mod_hi, mod_lo);
                planes.push(AddDmabufPlane {
                    fd,
                    plane_idx,
                    offset,
                    stride,
                    modifier,
                });
            }
            let mirror =
                vulkan_import_dmabuf(&vulk, planes, w as u32, h as u32, fmt, false).unwrap();

            let tmp = Arc::new(vulkan_get_buffer(&vulk, img.nominal_size(None), true).unwrap());
            copy_onto_dmabuf(&img, &tmp, &img_data).unwrap();

            if immed {
                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wl_surface_attach(dst, surface, buffer, 0, 0);
                    write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                    write_req_wl_surface_commit(dst, surface);
                }));
            } else {
                ctx.comp_write_passthrough(build_msgs(|dst| {
                    write_evt_zwp_linux_buffer_params_v1_created(dst, params, sbuffer);
                }));
                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wl_surface_attach(dst, surface, sbuffer, 0, 0);
                    write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                    write_req_wl_surface_commit(dst, surface);
                }));
            }

            let tmp = Arc::new(vulkan_get_buffer(&vulk, mirror.nominal_size(None), true).unwrap());
            let mir_data = copy_from_dmabuf(&img, &tmp).unwrap();
            assert!(img_data == mir_data);

            /* Cleanup buffer and params objects, for reuse with next image */
            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_zwp_linux_buffer_params_v1_destroy(dst, params);
                write_req_wl_buffer_destroy(dst, if immed { buffer } else { sbuffer });
            }));
            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_display_delete_id(dst, display, params.0);
                if immed {
                    write_evt_wl_display_delete_id(dst, display, buffer.0);
                }
            }));
        }
    });
}

#[cfg(feature = "video")]
fn fill_blocks_xrgb(w: usize, h: usize) -> Vec<u8> {
    let bpp = 4;
    let colors: [u32; 4] = [0xaf0080ff, 0xbf00ff00, 0xcf800080, 0xdf808080];
    let mut img_data = vec![0u8; (w * h) as usize * bpp];
    for y in 0..h {
        for x in 0..w {
            let by = (2 * y) / h;
            let bx = (2 * x) / w;
            let c = colors[2 * by + bx] ^ (((x * y) % 2) as u32) * 0x00010101;
            img_data[bpp * (y * w + x)..bpp * (y * w + x) + bpp].copy_from_slice(&c.to_le_bytes());
        }
    }
    img_data
}

#[cfg(feature = "video")]
fn fill_pseudorand_xrgb(w: usize, h: usize) -> Vec<u8> {
    let bpp = 4;
    let mut img_data = vec![0u8; (w * h) as usize * bpp];
    let mut a: u32 = 1;
    for y in 0..h {
        for x in 0..w {
            a = (2 * a) % 16777213;
            let c = a | 0xff000000;
            img_data[bpp * (y * w + x)..bpp * (y * w + x) + bpp].copy_from_slice(&c.to_le_bytes());
        }
    }
    img_data
}

#[cfg(feature = "dmabuf")]
fn create_dmabuf_and_copy(
    vulk: &Arc<Vulkan>,
    ctx: &mut ProtocolTestContext,
    params: ObjId,
    dmabuf: ObjId,
    buffer: ObjId,
    w: usize,
    h: usize,
    fmt: u32,
    initial_data: &[u8],
) -> Result<(Arc<VulkanDmabuf>, Arc<VulkanDmabuf>), String> {
    let modifier_list = vulk.get_supported_modifiers(fmt);
    let (img, planes) =
        vulkan_create_dmabuf(&vulk, w as u32, h as u32, fmt, &modifier_list, false).unwrap();

    /* Initialize the image with garbage data */
    let tmp_img = Arc::new(vulkan_get_buffer(&vulk, img.nominal_size(None), false).unwrap());
    copy_onto_dmabuf(&img, &tmp_img, initial_data).unwrap();

    let msgs = build_msgs(|dst| {
        write_req_zwp_linux_dmabuf_v1_create_params(dst, dmabuf, params);
        for p in planes.iter() {
            let (mod_hi, mod_lo) = split_u64(p.modifier);
            write_req_zwp_linux_buffer_params_v1_add(
                dst,
                params,
                true,
                p.plane_idx,
                p.offset,
                p.stride,
                mod_hi,
                mod_lo,
            );
        }

        write_req_zwp_linux_buffer_params_v1_create_immed(
            dst, params, buffer, w as i32, h as i32, fmt, 0,
        );
    });
    let plane_fds: Vec<&OwnedFd> = planes.iter().map(|p| &p.fd).collect();
    let (rmsgs, mut rfds) = ctx
        .prog_write(&msgs[..], &plane_fds[..])
        .map_err(|_| tag!("Failed to replicate dmabuf"))?;
    drop(planes);

    let add_msgs = &rmsgs[1..rmsgs.len() - 1];
    assert!(rfds.len() == add_msgs.len());
    let create_msg = &rmsgs[rmsgs.len() - 1];
    let (_rbuf, rw, rh, rfmt, _rflags) =
        parse_req_zwp_linux_buffer_params_v1_create_immed(&create_msg[..]).unwrap();
    assert!((rw, rh, rfmt) == (w as i32, h as i32, fmt));

    let mut planes = Vec::new();
    for (fd, msg) in rfds.drain(..).zip(add_msgs.iter()) {
        let (plane_idx, offset, stride, mod_hi, mod_lo) =
            parse_req_zwp_linux_buffer_params_v1_add(msg).unwrap();
        let modifier = join_u64(mod_hi, mod_lo);
        planes.push(AddDmabufPlane {
            fd,
            plane_idx,
            offset,
            stride,
            modifier,
        });
    }

    let mirror = vulkan_import_dmabuf(&vulk, planes, w as u32, h as u32, fmt, false).unwrap();
    Ok((img, mirror))
}

#[cfg(feature = "video")]
fn test_dmavid_inner(vulk: &Arc<Vulkan>, opts: &Options) -> bool {
    let pass = AtomicBool::new(true);
    run_protocol_test_with_opts(
        &|mut ctx: ProtocolTestContext| {
            let (display, registry, dmabuf, comp, surface, feedback, params, buffer) = (
                ObjId(1),
                ObjId(2),
                ObjId(3),
                ObjId(4),
                ObjId(5),
                ObjId(6),
                ObjId(7),
                ObjId(8),
            );

            setup_linux_dmabuf(
                &mut ctx, &vulk, display, registry, dmabuf, comp, surface, feedback,
            );

            // todo: run these in parallel?

            // The small (width <= 32, height <= 16 test sizes fail) with uniform (88)
            // green channel. height=16 shows nonuniform output but still has high error
            for (w, h) in [(64, 64), (257, 240), (1, 1), (11, 200), (201, 10)] {
                println!("Testing image transfer for WxH: {}x{}", w, h);

                let fmt = wayland_to_drm(WlShmFormat::Xrgb8888);

                let seed = fill_pseudorand_xrgb(w, h);
                let (img, mirror) = match create_dmabuf_and_copy(
                    vulk, &mut ctx, params, dmabuf, buffer, w, h, fmt, &seed,
                ) {
                    Ok(x) => x,
                    Err(y) => {
                        println!("Failed to create and replicate dmabuf: {:?}", y);
                        pass.store(false, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                };

                let img_data = fill_blocks_xrgb(w, h);
                let tmp_img =
                    Arc::new(vulkan_get_buffer(&vulk, img.nominal_size(None), true).unwrap());
                copy_onto_dmabuf(&img, &tmp_img, &img_data).unwrap();

                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wl_surface_attach(dst, surface, buffer, 0, 0);
                    write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                    write_req_wl_surface_commit(dst, surface);
                }));

                let tmp_mirror =
                    Arc::new(vulkan_get_buffer(&vulk, mirror.nominal_size(None), true).unwrap());
                let mir_data = copy_from_dmabuf(&mirror, &tmp_mirror).unwrap();

                let mut net_diff: u64 = 0;
                for (px_i, px_m) in img_data.chunks_exact(4).zip(mir_data.chunks_exact(4)) {
                    let (ib, ig, ir, _ix) = (px_i[0], px_i[1], px_i[2], px_i[3]);
                    let (mb, mg, mr, _mx) = (px_m[0], px_m[1], px_m[2], px_m[3]);
                    net_diff +=
                        ib.abs_diff(mb) as u64 + ig.abs_diff(mg) as u64 + ir.abs_diff(mr) as u64;
                }
                let avg_diff = net_diff / ((w * h) as u64);
                /* Allow up to +/- 32 error on flat regions, and +/- 255 on edges between colored blocks */
                let mut threshold: u64 = 32 + (255 - 32) / (w as u64) + (255 - 32) / (h as u64);
                if w == 1 && h == 1 {
                    threshold = 32;
                }
                if net_diff == 0 {
                    /* Either lossless video, or no video encoding at all */
                    println!(
                        "Perfect replication of {}x{} image, likely not using video enc/decoding",
                        w, h
                    );
                } else {
                    println!(
                        "Average difference on slightly noisy {}x{} block pattern: {}, threshold {}",
                        w,
                        h,
                        (net_diff as f32) / ((w * h) as f32),
                            threshold,
                    );
                    if avg_diff >= threshold {
                        println!("Green channel");
                        for y in 0..h {
                            for x in 0..w {
                                let base: [u8; 4] = img_data
                                    [(4 * y * w + 4 * x)..(4 * y * w + 4 * x + 4)]
                                    .try_into()
                                    .unwrap();
                                let rep: [u8; 4] = mir_data
                                    [(4 * y * w + 4 * x)..(4 * y * w + 4 * x + 4)]
                                    .try_into()
                                    .unwrap();
                                print!("{:2x}/{:2x} ", base[1], rep[1]);
                            }
                            println!();
                        }
                    }
                }

                if avg_diff > threshold {
                    pass.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                // TODO: once video bugs are resolved, set this
                // assert!(avg_diff < threshold);

                /* Cleanup buffer and params objects, for reuse with next image */
                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_zwp_linux_buffer_params_v1_destroy(dst, params);
                    write_req_wl_buffer_destroy(dst, buffer);
                }));
                ctx.comp_write_passthrough(build_msgs(|dst| {
                    write_evt_wl_display_delete_id(dst, display, params.0);
                    write_evt_wl_display_delete_id(dst, display, buffer.0);
                }));
            }
        },
        opts,
        None,
    );

    pass.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(feature = "video")]
fn test_video_combo(
    vulk: &Arc<Vulkan>,
    video_format: VideoFormat,
    try_hw_dec: bool,
    try_hw_enc: bool,
    allow_fail: bool,
) {
    let opts = Options {
        debug: false, /* validation layers and libavcodec write some uncaptured logging messages */
        compression: Compression::None,
        video: VideoSetting {
            format: Some(video_format),
            bits_per_frame: None, // note: very high/low values can cause codec failure
        },
        threads: 1,
        title_prefix: String::new(),
        no_gpu: false,
        drm_node: None,
        force_sw_encoding: !try_hw_enc,
        force_sw_decoding: !try_hw_dec,
    };
    println!(
        "\nTrying combination: video={:?}, try_hw_dec={}, try_hw_enc={}",
        video_format, try_hw_dec, try_hw_enc
    );
    let pass = test_dmavid_inner(&vulk, &opts);
    println!(
        "Result for video={:?}, try_hw_dec={}, try_hw_enc={}: {}",
        video_format,
        try_hw_dec,
        try_hw_enc,
        if pass { "pass" } else { "fail" }
    );

    // TODO: hardware video decoding of w<=32, h<=16 (w=32,h=16) images appears broken
    if !allow_fail {
        assert!(pass);
    }
}

#[cfg(feature = "video")]
#[test]
fn proto_dmavid_vp9() {
    let vulk = setup_vulkan(None, false, true, false, false).unwrap();
    test_video_combo(&vulk, VideoFormat::VP9, false, false, false);
}

#[cfg(feature = "video")]
#[test]
fn proto_dmavid_h264() {
    let vulk = setup_vulkan(None, false, true, false, false).unwrap();

    /* Test all hardware encoding/decoding combinations to sure formats are compatible */
    for (try_hw_dec, try_hw_enc, allow_fail) in [
        (false, false, false),
        (true, false, true),
        (true, true, true),
        (false, true, false),
    ] {
        test_video_combo(&vulk, VideoFormat::H264, try_hw_dec, try_hw_enc, allow_fail);
    }
}

#[test]
fn proto_oversized() {
    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (display, registry) = (ObjId(1), ObjId(2));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }));

        let msg_len = (1 << 16) - 4;
        let str_length = msg_len - length_evt_wl_registry_global(0);
        assert!(length_evt_wl_registry_global(str_length) == msg_len);
        let mut msg = vec![0; msg_len];
        let mut dst = &mut msg[..];
        let long_name = vec![b'a'; str_length];
        write_evt_wl_registry_global(&mut dst, registry, 1, &long_name, 1);
        println!(
            "header: {:?}, {}",
            parse_wl_header(&msg),
            length_evt_wl_registry_global(0)
        );

        /* Waypipe should either reject or pass the message, but not hang or crash */
        let res = ctx.comp_write(&msg, &[]);
        assert!(res.is_err() || res.is_ok_and(|x| x.0.concat() == msg));
    });
}

fn get_diff_damage(
    iter: usize,
    base: &mut [u8],
    w: usize,
    h: usize,
    stride: usize,
    bpp: usize,
) -> Vec<(i32, i32, i32, i32)> {
    let iw: i32 = w.try_into().unwrap();
    let ih: i32 = h.try_into().unwrap();
    match iter {
        0 => {
            /* Test: large disjoint blocks, left side */
            for y in 0..h / 4 {
                for x in 0..w / 4 {
                    base[(y * stride + bpp * x)..(y * stride + bpp * x + bpp)].fill(0x40);
                }
            }
            for y in (3 * h) / 4..h {
                for x in 0..w / 4 {
                    base[(y * stride + bpp * x)..(y * stride + bpp * x + bpp)].fill(0x20);
                }
            }
            vec![
                (0, 0, iw / 4, ih / 4),
                (0, (3 * ih) / 4, iw / 4, (ih - (3 * ih) / 4)),
            ]
        }
        1 => {
            /* Test: sparse differences in middle */
            for y in (3 * h) / 8..(5 * h) / 8 {
                let x = y.clamp(w / 8, (7 * w) / 8);
                base[y * stride + bpp * x + bpp / 2] = 0xa0;
            }
            vec![(0, (3 * ih) / 8, iw, (5 * ih) / 8 - (3 * ih) / 8)]
        }
        2 => {
            /* Test: large disjoint blocks, right side */
            for y in 0..h / 4 {
                for x in (w / 8)..w {
                    base[(y * stride + bpp * x)..(y * stride + bpp * x + bpp)].fill(0x40);
                }
            }
            for y in (3 * h) / 4..h {
                for x in (w / 8)..w {
                    base[(y * stride + bpp * x)..(y * stride + bpp * x + bpp)].fill(0x30);
                }
            }
            vec![
                (0, 0, iw, ih / 4),
                (0, (3 * ih) / 4, iw, (ih - (3 * ih) / 4)),
            ]
        }
        _ => unreachable!(),
    }
}

#[test]
fn proto_shm_damage() {
    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (display, registry, shm, comp, surface, pool, buffer) = (
            ObjId(1),
            ObjId(2),
            ObjId(3),
            ObjId(4),
            ObjId(5),
            ObjId(6),
            ObjId(7),
        );

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }));

        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, "wl_shm".as_bytes(), 2);
            write_evt_wl_registry_global(dst, registry, 2, "wl_compositor".as_bytes(), 6);
        }));

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, "wl_shm".as_bytes(), 2, shm);
            write_req_wl_registry_bind(dst, registry, 2, "wl_compositor".as_bytes(), 6, comp);
            write_req_wl_compositor_create_surface(dst, comp, surface);
        }));

        /* For wl_shm buffer replication, the format is not very important as it only affects damage calculations */
        let (w, h) = (257, 257);
        for format in [WlShmFormat::Rgb565, WlShmFormat::Argb8888] {
            let bpp = match format {
                WlShmFormat::Argb8888 => 4,
                WlShmFormat::Rgb565 => 2,
                _ => unreachable!(),
            };
            let stride = align(bpp * w, 19);
            let file_sz = h * stride;
            let mut base = vec![0x80; file_sz];
            let buffer_fd = make_file_with_contents(&base).unwrap();
            let msg = build_msgs(|dst| {
                write_req_wl_shm_create_pool(dst, shm, true, pool, file_sz as i32);
                write_req_wl_shm_pool_create_buffer(
                    dst,
                    pool,
                    buffer,
                    0,
                    w as i32,
                    h as i32,
                    stride as i32,
                    format as u32,
                );
                write_req_wl_shm_pool_destroy(dst, pool);
                write_req_wl_surface_attach(dst, surface, buffer, 0, 0);
                write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                write_req_wl_surface_commit(dst, surface);
            });
            let (rmsg, mut rfd) = ctx.prog_write(&msg[..], &[&buffer_fd]).unwrap();
            assert!(rmsg.concat() == msg);
            assert!(rfd.len() == 1);
            let output_fd = rfd.remove(0);
            assert!(get_file_contents(&output_fd, file_sz).unwrap() == base);

            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_display_delete_id(dst, display, pool.0);
            }));
            for iter in 0..3 {
                let damage: Vec<(i32, i32, i32, i32)> =
                    get_diff_damage(iter, &mut base, w, h, stride, bpp);
                update_file_contents(&buffer_fd, &base).unwrap();

                ctx.prog_write_passthrough(build_msgs(|dst| {
                    for d in damage {
                        write_req_wl_surface_damage_buffer(dst, surface, d.0, d.1, d.2, d.3);
                    }
                    write_req_wl_surface_commit(dst, surface);
                }));

                assert!(get_file_contents(&output_fd, file_sz).unwrap() == base);

                ctx.comp_write_passthrough(build_msgs(|dst| {
                    write_evt_wl_buffer_release(dst, buffer);
                }));
            }

            ctx.prog_write_passthrough(build_msgs(|dst| {
                write_req_wl_buffer_destroy(dst, buffer);
            }));
            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_display_delete_id(dst, display, buffer.0);
            }));
        }
    });
}

#[cfg(feature = "dmabuf")]
#[test]
fn proto_dmabuf_damage() {
    let vulk = setup_vulkan(None, false, true, false, false).unwrap();

    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (display, registry, dmabuf, comp, surface, feedback, params, buffer) = (
            ObjId(1),
            ObjId(2),
            ObjId(3),
            ObjId(4),
            ObjId(5),
            ObjId(6),
            ObjId(7),
            ObjId(8),
        );

        setup_linux_dmabuf(
            &mut ctx, &vulk, display, registry, dmabuf, comp, surface, feedback,
        );

        /* For dmabuf replication, the format (well, texel size) affects diff alignment */
        for (w, h) in [(64_usize, 64_usize), (257_usize, 257_usize)] {
            for wl_format in [
                WlShmFormat::R8,
                WlShmFormat::Rgb565,
                WlShmFormat::Argb8888,
                WlShmFormat::Argb16161616,
            ] {
                let bpp = match wl_format {
                    WlShmFormat::Argb16161616 => 8,
                    WlShmFormat::Argb8888 => 4,
                    WlShmFormat::Rgb565 => 2,
                    WlShmFormat::R8 => 1,
                    _ => unreachable!(),
                };
                let format = wayland_to_drm(wl_format);
                let stride = bpp * w;
                let file_sz = h * stride;
                let mut base = vec![0x80; file_sz];

                let (img, mirror) = create_dmabuf_and_copy(
                    &vulk, &mut ctx, params, dmabuf, buffer, w, h, format, &base,
                )
                .unwrap();

                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wl_surface_attach(dst, surface, buffer, 0, 0);
                    write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                    write_req_wl_surface_commit(dst, surface);
                }));

                let tmp_wr =
                    Arc::new(vulkan_get_buffer(&vulk, img.nominal_size(None), false).unwrap());
                let tmp_rd =
                    Arc::new(vulkan_get_buffer(&vulk, img.nominal_size(None), true).unwrap());

                for iter in 0..3 {
                    let damage: Vec<(i32, i32, i32, i32)> =
                        get_diff_damage(iter, &mut base, w, h, stride, bpp);
                    copy_onto_dmabuf(&img, &tmp_wr, &base).unwrap();

                    ctx.prog_write_passthrough(build_msgs(|dst| {
                        for d in damage {
                            write_req_wl_surface_damage_buffer(dst, surface, d.0, d.1, d.2, d.3);
                        }
                        write_req_wl_surface_commit(dst, surface);
                    }));

                    let dup = copy_from_dmabuf(&mirror, &tmp_rd).unwrap();
                    assert!(
                        dup == base,
                        "{} {}\n{:?}\n{:?}",
                        dup.len(),
                        base.len(),
                        dup,
                        base
                    );

                    ctx.comp_write_passthrough(build_msgs(|dst| {
                        write_evt_wl_buffer_release(dst, buffer);
                    }));
                }

                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_zwp_linux_buffer_params_v1_destroy(dst, params);
                    write_req_wl_buffer_destroy(dst, buffer);
                }));
                ctx.comp_write_passthrough(build_msgs(|dst| {
                    write_evt_wl_display_delete_id(dst, display, params.0);
                    write_evt_wl_display_delete_id(dst, display, buffer.0);
                }));
            }
        }
    });
}

#[cfg(feature = "dmabuf")]
#[test]
fn proto_explicit_sync() {
    let vulk = setup_vulkan(None, false, true, false, false).unwrap();

    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        let (
            display,
            registry,
            dmabuf,
            comp,
            surface,
            feedback,
            params,
            manager,
            sync_surf,
            timeline,
            buffer,
        ) = (
            ObjId(1),
            ObjId(2),
            ObjId(3),
            ObjId(4),
            ObjId(5),
            ObjId(6),
            ObjId(7),
            ObjId(8),
            ObjId(9),
            ObjId(10),
            ObjId(11),
        );

        setup_linux_dmabuf(
            &mut ctx, &vulk, display, registry, dmabuf, comp, surface, feedback,
        );
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_registry_global(
                dst,
                registry,
                3,
                "wp_linux_drm_syncobj_manager_v1".as_bytes(),
                1,
            );
        }));
        let msg = build_msgs(|dst| {
            write_req_wl_registry_bind(
                dst,
                registry,
                3,
                "wp_linux_drm_syncobj_manager_v1".as_bytes(),
                1,
                manager,
            );
            write_req_wp_linux_drm_syncobj_manager_v1_get_surface(dst, manager, sync_surf, surface);
            write_req_wp_linux_drm_syncobj_manager_v1_import_timeline(dst, manager, true, timeline);
        });
        let start_pt = 150;
        let (prog_timeline, timeline_fd) = vulkan_create_timeline(&vulk, start_pt).unwrap();

        let (rmsg, mut rfd) = ctx.prog_write(&msg[..], &[&timeline_fd]).unwrap();
        assert!(rmsg.concat() == msg);
        drop(timeline_fd);
        assert!(rfd.len() == 1);
        let output_fd = rfd.remove(0);

        let comp_timeline = vulkan_import_timeline(&vulk, output_fd).unwrap();

        let (w, h) = (512, 512);
        let format = wayland_to_drm(WlShmFormat::R8);
        let file_sz = h * w;
        let mut base = vec![0x80; file_sz];

        let (img, mirror) =
            create_dmabuf_and_copy(&vulk, &mut ctx, params, dmabuf, buffer, w, h, format, &base)
                .unwrap();

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_surface_attach(dst, surface, buffer, 0, 0);
            write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
            write_req_wl_surface_commit(dst, surface);
        }));

        let tmp_wr = Arc::new(vulkan_get_buffer(&vulk, img.nominal_size(None), false).unwrap());
        let tmp_rd = Arc::new(vulkan_get_buffer(&vulk, img.nominal_size(None), true).unwrap());

        for iter in 0..4 {
            /* Change entire image, except on the second iteration */
            if iter != 1 {
                base.fill(iter);
            }
            copy_onto_dmabuf(&img, &tmp_wr, &base).unwrap();
            let acq_pt: u64 = start_pt + 11 + (iter as u64) * 7;
            let rel_pt = acq_pt + 2;

            if iter == 2 {
                /* Special case: signal before writing */
                println!("Signalling acquire {}", acq_pt);
                prog_timeline.signal_timeline_pt(acq_pt).unwrap();
            }

            let msg = build_msgs(|dst| {
                write_req_wl_surface_damage_buffer(dst, surface, 0, 0, w as i32, h as i32);
                let (acq_hi, acq_lo) = split_u64(acq_pt);
                let (rel_hi, rel_lo) = split_u64(rel_pt);
                write_req_wp_linux_drm_syncobj_surface_v1_set_acquire_point(
                    dst, sync_surf, timeline, acq_hi, acq_lo,
                );
                write_req_wp_linux_drm_syncobj_surface_v1_set_release_point(
                    dst, sync_surf, timeline, rel_hi, rel_lo,
                );
                write_req_wl_surface_commit(dst, surface);
            });

            test_write_msgs(&ctx.sock_prog, &msg, &[]);

            /* Signal after writing; this is safe because the messages sent will at minimum
             * fit in the pipe buffer */
            if iter != 2 {
                println!("Signalling acquire {}", acq_pt);
                prog_timeline.signal_timeline_pt(acq_pt).unwrap();
            }

            /* Only start reading messages after signalling; this prevents a possible deadlock,
             * because the code might wait for the signal before sending messages further */
            let (rmsg, rfds, err) = test_read_msgs(&ctx.sock_comp, Some(&ctx.sock_prog));
            assert!(err.is_none());
            assert!(rfds.is_empty());
            assert!(rmsg.concat() == msg);

            let max_wait = 1000000000;
            println!("Waiting for acquire {}", acq_pt);
            comp_timeline
                .wait_for_timeline_pt(acq_pt, max_wait)
                .unwrap();

            let dup = copy_from_dmabuf(&mirror, &tmp_rd).unwrap();
            assert!(dup == base);

            println!("Signalling release {}", rel_pt);
            comp_timeline.signal_timeline_pt(rel_pt).unwrap();
            println!("Waiting for release {}", rel_pt);
            prog_timeline
                .wait_for_timeline_pt(rel_pt, max_wait)
                .unwrap();

            ctx.comp_write_passthrough(build_msgs(|dst| {
                write_evt_wl_buffer_release(dst, buffer);
            }));
        }

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_zwp_linux_buffer_params_v1_destroy(dst, params);
            write_req_wl_buffer_destroy(dst, buffer);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_display_delete_id(dst, display, params.0);
            write_evt_wl_display_delete_id(dst, display, buffer.0);
        }));
    });
}

#[test]
fn proto_many_fds() {
    /* Check that Waypipe can successfully process a large number of FDS */

    let mut files: Vec<(Vec<u8>, OwnedFd)> = Vec::new();
    /* 100 is the > the 28 max fds sent in a batch by libwayland, but also
     * not that large that having four copies of each would break a standard
     * 1024-fd ulimit. */
    for i in 0..100 {
        let x: Vec<u8> = format!("{}", i).into();
        let fd = make_file_with_contents(&x).unwrap();
        files.push((x, fd));
    }

    run_protocol_test(&|mut ctx: ProtocolTestContext| {
        /* Setup a wl_keyboard */
        let (display, registry, seat, keyboard) = (ObjId(1), ObjId(2), ObjId(3), ObjId(4));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, "wl_seat".as_bytes(), 7);
        }));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, "wl_seat".as_bytes(), 7, seat);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_seat_capabilities(dst, seat, 3);
        }));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_seat_get_keyboard(dst, seat, keyboard);
        }));

        /* Send all the files, in order */
        let fds: Vec<&OwnedFd> = files.iter().map(|(_c, f)| f).collect();

        let m = &build_msgs(|dst| {
            for (contents, _fd) in &files {
                write_evt_wl_keyboard_keymap(dst, keyboard, true, 1, contents.len() as u32);
            }
        });

        let (msgs, ofds) = ctx.comp_write(m, &fds).unwrap();

        assert!(msgs.concat() == *m);
        assert!(msgs.len() == ofds.len() && msgs.len() == files.len());
        for ((_msg, rfd), (contents, _fd)) in
            msgs.into_iter().zip(ofds.into_iter()).zip(files.iter())
        {
            let v = get_file_contents(&rfd, contents.len()).unwrap();
            assert!(v == *contents);
        }
    });
}

#[test]
fn proto_title_prefix() {
    // /* test lengths are: empty, short, too long for small buffers, cannot fit in a Wayland message */
    for (prefix_len, fail) in [(0, false), (100, false), (10000, true), (100000, true)] {
        let options = Options {
            debug: true,
            compression: Compression::None,
            video: VideoSetting {
                format: None,
                bits_per_frame: None,
            },
            threads: 1,
            title_prefix: String::from("a").repeat(prefix_len),
            no_gpu: false,
            drm_node: None,
            force_sw_encoding: false,
            force_sw_decoding: false,
        };
        run_protocol_test_with_opts(
            &|mut ctx: ProtocolTestContext| {
                const ID_SEQUENCE: [ObjId; 7] = [
                    ObjId(1),
                    ObjId(2),
                    ObjId(3),
                    ObjId(4),
                    ObjId(5),
                    ObjId(6),
                    ObjId(7),
                ];
                let [display, reg, compositor, xdg_wm_base, wl_surf, xdg_surf, toplevel, ..] =
                    ID_SEQUENCE;

                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wl_display_get_registry(dst, display, reg);
                }));
                ctx.comp_write_passthrough(build_msgs(|dst| {
                    write_evt_wl_registry_global(dst, reg, 1, "wl_compositor".as_bytes(), 6);
                    write_evt_wl_registry_global(dst, reg, 2, "xdg_wm_base".as_bytes(), 6);
                }));
                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wl_registry_bind(
                        dst,
                        reg,
                        1,
                        "wl_compositor".as_bytes(),
                        6,
                        compositor,
                    );
                    write_req_wl_registry_bind(
                        dst,
                        reg,
                        2,
                        "xdg_wm_base".as_bytes(),
                        6,
                        xdg_wm_base,
                    );
                    write_req_wl_compositor_create_surface(dst, compositor, wl_surf);
                    write_req_xdg_wm_base_get_xdg_surface(dst, xdg_wm_base, xdg_surf, wl_surf);
                    write_req_xdg_surface_get_toplevel(dst, xdg_surf, toplevel);
                }));
                let title_lengths = [0, 200];
                let test = build_msgs(|dst| {
                    for title_len in title_lengths {
                        let title = vec!['b' as u8; title_len];
                        write_req_xdg_toplevel_set_title(dst, toplevel, &title);
                    }
                });

                match ctx.prog_write(&test, &[]) {
                    Err(_) => {
                        assert!(fail);
                    }
                    Ok((rmsgs, rfds)) => {
                        assert!(!fail);
                        assert!(rfds.is_empty());
                        for (title_len, mut msg) in title_lengths.iter().zip(rmsgs.iter()) {
                            assert!(
                                parse_wl_header(&msg)
                                    == (toplevel, msg.len(), OPCODE_XDG_TOPLEVEL_SET_TITLE.code())
                            );
                            let title = parse_req_xdg_toplevel_set_title(&mut msg).unwrap();
                            let orig_title = vec!['b' as u8; *title_len];
                            /* The title prefix is added twice, once per main loop instance */
                            let mut ref_title = vec!['a' as u8; prefix_len * 2];
                            ref_title.extend_from_slice(&orig_title);
                            assert!(title == ref_title);
                        }
                    }
                }
            },
            &options,
            None,
        );
    }
}
