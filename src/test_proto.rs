/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! Protocol test runner and framework.
 *
 * Emulate a Wayland client/compositor pair and verify that Waypipe properly
 * forwards messages and file descriptors.
 *
 * This is needed to:
 * - Properly capture debug output from processes under test
 * - Run Waypipe instances as individual processes instead of threads (to avoid
 *   possible bugs if Vulkan validation layers are used for independent instances)
 * - Test different versions of Waypipe against each other.
 * - Break out test variants by the Vulkan physical device being used
 */

use clap::{value_parser, Arg, ArgAction, Command as ClapCommand};
use nix::libc;
use nix::sys::wait::WaitStatus;
use nix::sys::{memfd, signal, socket, time, wait};
use nix::{errno::Errno, fcntl, poll, unistd};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod dmabuf;
#[allow(dead_code)]
mod kernel;
#[allow(dead_code)]
mod util;
#[allow(dead_code)]
mod video; /* Only included because it is required by 'video' */
#[allow(dead_code)]
mod wayland;
mod wayland_gen;

#[cfg(feature = "dmabuf")]
use dmabuf::*;
use kernel::*;
use util::*;
use wayland::*;
use wayland_gen::*;

#[derive(Debug)]
enum StatusOk {
    Pass,
    Skipped,
}
#[derive(Debug)]
enum StatusBad {
    Unclear(String),
    Fail(String),
}
/** Result of a test, organized so that StatusBad can be progated with ? */
type TestResult = Result<StatusOk, StatusBad>;

#[derive(Debug)]
enum TestCategory {
    /** Test passed */
    Pass,
    /** Test was skipped. */
    Skipped,
    /** Test failed */
    Fail,
    /** Something was broken, but it may be an issue in a library used by Waypipe */
    Unclear,
}
/** 77 is the automake return code for a skipped test */
const EXITCODE_SKIPPED: u8 = 77;
/** 99 is the automake return code for a hard error (failure of
 * test set up, segfault, or something else very unexpected) */
const EXITCODE_UNCLEAR: u8 = 99;

struct TestInfo<'a> {
    test_name: &'a str,
    waypipe_client: &'a OsStr,
    waypipe_server: &'a OsStr,
}

/* Constant, saves some code when allocating sequence ids */
const ID_SEQUENCE: [ObjId; 10] = [
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
];

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
    assert!(data.len() >= fds.len().div_ceil(MAX_FDS_PER_BYTE));

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
                let (obj, code, msg) = parse_evt_wl_display_error(msg).unwrap();
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
            iovs,
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
            fds.append(&mut recv[0].fds);
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

fn build_msgs<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut &mut [u8]),
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

struct WaypipeOptions<'a> {
    wire_version: Option<u32>,
    drm_node: Option<u64>,
    video: VideoSetting,
    title_prefix: &'a str,
    compression: Compression,
}

fn build_arguments(waypipe_bin: &OsStr, opts: &WaypipeOptions, is_client: bool) -> Vec<String> {
    let mut v = Vec::new();

    let cross_runner = std::env::var_os("CROSS_TARGET_RUNNER");
    let cx: &OsStr = cross_runner.as_deref().unwrap_or_default();
    /* note: CROSS_TARGET_RUNNER is typically something like 'CROSS_TARGET_RUNNER=/linux-runner aarch64' */
    for chunk in cx.as_encoded_bytes().split(|x| *x == b' ') {
        if !chunk.is_empty() {
            v.push(std::str::from_utf8(chunk).unwrap().into());
        }
    }

    v.push(waypipe_bin.to_str().unwrap().into());
    v.push("--debug".into());
    v.push("--threads=1".into());
    v.push(format!("--compress={}", opts.compression));
    if !opts.title_prefix.is_empty() {
        v.push(format!("--title-prefix={}", opts.title_prefix));
    }
    if let Some(device_id) = opts.drm_node {
        v.push(format!("--drm-node=/dev/dri/renderD{}", (device_id & 0xff)));
    } else {
        v.push("--no-gpu".into());
    }
    if opts.video.format.is_some() {
        v.push(format!("--video={}", opts.video));
    }
    if let Some(ver) = opts.wire_version {
        v.push(format!("--test-wire-version={}", ver));
    }

    if is_client {
        v.push("client-conn".into());
    } else {
        v.push("server-conn".into());
    }
    v
}

fn run_protocol_test_with_opts(
    info: &TestInfo,
    opts_client: &WaypipeOptions,
    opts_server: &WaypipeOptions,
    test_fn: &dyn Fn(ProtocolTestContext),
) -> Result<(), StatusBad> {
    let (channel1, channel2) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_CLOEXEC,
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

    let client_args = build_arguments(info.waypipe_client, opts_client, true);
    let server_args = build_arguments(info.waypipe_server, opts_server, false);

    set_cloexec(&prog_comp2, false).unwrap();
    set_cloexec(&channel1, false).unwrap();
    let mut client: Child = Command::new(&client_args[0])
        .args(&client_args[1..])
        .env("WAYLAND_SOCKET", format!("{}", prog_comp2.as_raw_fd()))
        .env("WAYPIPE_CONNECTION_FD", format!("{}", channel1.as_raw_fd()))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    drop(prog_comp2);
    drop(channel1);

    set_cloexec(&prog_appl2, false).unwrap();
    set_cloexec(&channel2, false).unwrap();
    let mut server: Child = Command::new(&server_args[0])
        .args(&server_args[1..])
        .env("WAYLAND_SOCKET", format!("{}", channel2.as_raw_fd()))
        .env(
            "WAYPIPE_CONNECTION_FD",
            format!("{}", prog_appl2.as_raw_fd()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    drop(prog_appl2);
    drop(channel2);

    let ctx = ProtocolTestContext {
        sock_prog: prog_appl1,
        sock_comp: prog_comp1,
    };
    test_fn(ctx);

    /* Wait for processes to die */
    // todo: this needs a 1 second timeout to properly catch deadlocked processes
    let exit_c = client.wait().unwrap();
    let exit_s = server.wait().unwrap();
    if !exit_s.success() || !exit_c.success() {
        let msg = tag!(
            "Waypipe connection handlers for {} did not exit cleanly: client {} server {}",
            info.test_name,
            exit_c.success(),
            exit_s.success()
        );
        return Err(StatusBad::Fail(msg));
    }

    Ok(())
}

#[cfg(feature = "dmabuf")]
fn run_protocol_test_with_drm_node(
    info: &TestInfo,
    device: &RenderDevice,
    test_fn: &dyn Fn(ProtocolTestContext),
) -> Result<(), StatusBad> {
    let options = WaypipeOptions {
        wire_version: None,
        drm_node: Some(device.id),
        compression: Compression::None,
        title_prefix: "",
        video: VideoSetting::default(),
    };
    run_protocol_test_with_opts(info, &options, &options, test_fn)
}

fn run_protocol_test(
    info: &TestInfo,
    test_fn: &dyn Fn(ProtocolTestContext),
) -> Result<(), StatusBad> {
    let options = WaypipeOptions {
        wire_version: None,
        drm_node: None,
        compression: Compression::None,
        title_prefix: "",
        video: VideoSetting::default(),
    };
    run_protocol_test_with_opts(info, &options, &options, test_fn)
}

fn get_intf_name(intf: WaylandInterface) -> &'static [u8] {
    INTERFACE_TABLE[intf as usize].name.as_bytes()
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
    unistd::ftruncate(fd, data.len().try_into().unwrap())
        .map_err(|x| tag!("Failed to resize memfd: {:?}", x))?;
    let mapping = ExternalMapping::new(fd, data.len(), false)?;
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

fn test_is_included(name: &str, filter: &[&str]) -> bool {
    if filter.is_empty() {
        return true;
    }
    for x in filter {
        if name.contains(x) {
            return true;
        }
    }
    false
}

fn register_single<'a>(
    tests: &mut Vec<(String, Box<dyn Fn(TestInfo) -> TestResult + 'a>)>,
    filter: &[&str],
    name: &str,
    func: fn(TestInfo) -> TestResult,
) {
    if !test_is_included(name, filter) {
        return;
    }

    tests.push((format!("proto::{}", name), Box::new(func)));
}

fn register_per_device<'a>(
    tests: &mut Vec<(String, Box<dyn Fn(TestInfo) -> TestResult + 'a>)>,
    filter: &[&str],
    devices: &[(String, u64)],
    name: &str,
    func: fn(TestInfo, RenderDevice) -> TestResult,
) {
    for (dev_name, dev_id) in devices {
        let ext_name = format!("proto::{}::{}", name, dev_name);
        if !test_is_included(&ext_name, filter) {
            return;
        }
        let m: (String, u64) = (dev_name.clone(), *dev_id);
        tests.push((
            ext_name,
            Box::new(move |info| {
                let s = &m;
                let dev = RenderDevice {
                    name: &s.0,
                    id: s.1,
                };
                func(info, dev)
            }),
        ));
    }
}

struct RenderDevice<'a> {
    #[allow(unused)]
    name: &'a str,
    id: u64,
}

pub fn list_vulkan_device_ids() -> Vec<(String, u64)> {
    use nix::sys::stat;
    use std::os::unix::ffi::OsStrExt;

    let mut dev_ids = Vec::new();
    let Ok(dir_iter) = std::fs::read_dir("/dev/dri") else {
        /* On failure, assume Vulkan is not available */
        return dev_ids;
    };

    for r in dir_iter {
        let std::io::Result::Ok(entry) = r else {
            continue;
        };
        if !entry.file_name().as_bytes().starts_with(b"renderD") {
            continue;
        }
        let Ok(result) = stat::stat(&entry.path()) else {
            continue;
        };
        /* st_rdev may be u32 on old architectures */
        #[allow(clippy::unnecessary_cast)]
        dev_ids.push((
            entry.file_name().into_string().unwrap(),
            result.st_rdev as u64,
        ));
    }
    dev_ids
}

/** No-op signal handler (used to ensure SIGCHLD interrupts poll) */
extern "C" fn noop_signal_handler(_: i32) {}

fn proto_basic(info: TestInfo) -> TestResult {
    /* Test to verify that a simple message exchange behaves as expected */
    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
    })?;
    Ok(StatusOk::Pass)
}

fn proto_base_wire(info: TestInfo) -> TestResult {
    /* Test that using the base protocol version still works, for basic operations */
    let opts = WaypipeOptions {
        wire_version: Some(MIN_PROTOCOL_VERSION),
        drm_node: None,
        title_prefix: "",
        compression: Compression::None,
        video: VideoSetting::default(),
    };
    run_protocol_test_with_opts(&info, &opts, &opts, &|mut ctx: ProtocolTestContext| {
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
    })?;
    Ok(StatusOk::Pass)
}

fn proto_keymap(info: TestInfo) -> TestResult {
    /* Test to verify that keymap files can be transferred reliably */
    for length in [0, 9, 4096, 300001] {
        run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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

            let (msgs, mut fds) = ctx
                .comp_write(
                    &build_msgs(|dst| {
                        write_evt_wl_keyboard_keymap(
                            dst,
                            keyboard,
                            false,
                            1,
                            source_data.len() as _,
                        );
                    }),
                    &[&source_fd],
                )
                .unwrap();
            drop(source_fd);

            let (_format, keymap_length) = parse_evt_wl_keyboard_keymap(&msgs[0]).unwrap();

            let new_kb_fd = fds.remove(0);
            let new_data = get_file_contents(&new_kb_fd, keymap_length as usize).unwrap();
            assert!(
                new_data == source_data,
                "{} {:?} {:?}",
                keymap_length,
                &new_data[..new_data.len().min(1000)],
                &source_data[..source_data.len().min(1000)]
            );
        })?;
    }
    Ok(StatusOk::Pass)
}

/* Send `data` into `src` and check that it comes out of `dst` */
fn check_pipe_transfer(pipe_w: OwnedFd, pipe_r: OwnedFd, data: &[u8]) {
    let mut nwritten = 0;
    let start = Instant::now();
    let timeout = Duration::from_secs(1);

    let mut ord = Some(pipe_r);
    let mut owr = Some(pipe_w);
    if data.is_empty() {
        owr = None;
    }

    let mut recv = Vec::new();

    let mut tmp = vec![0; 4096];
    while ord.is_some() || owr.is_some() {
        let mut pfds = Vec::new();
        let wr_idx = owr.as_ref().map(|x| {
            pfds.push(poll::PollFd::new(x.as_fd(), poll::PollFlags::POLLOUT));
            pfds.len() - 1
        });
        let rd_idx = ord.as_ref().map(|x| {
            pfds.push(poll::PollFd::new(x.as_fd(), poll::PollFlags::POLLIN));
            pfds.len() - 1
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
        let rev_wr = wr_idx.map(|i| pfds[i].revents().unwrap());
        let rev_rd = rd_idx.map(|i| pfds[i].revents().unwrap());

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

fn proto_pipe_write(info: TestInfo) -> TestResult {
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
            write_evt_wl_registry_global(dst, registry, 2, ddev_name, 3);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, ddev_name, 3, manager);
            write_req_wl_data_device_manager_get_data_device(dst, manager, dev, seat);
            write_req_wl_data_device_manager_create_data_source(dst, manager, source);
            write_req_wl_data_source_offer(dst, source, mime);
            write_req_wl_data_device_set_selection(dst, dev, source, 99);
        }),
        build_msgs(|dst| {
            write_evt_wl_data_source_send(dst, source, false, mime);
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
            write_evt_zwp_primary_selection_source_v1_send(dst, source, false, mime);
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
            write_evt_ext_data_control_source_v1_send(dst, source, false, mime);
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
            write_evt_gtk_primary_selection_source_send(dst, source, false, mime);
        }),
    ];
    let ex_wlr: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, wlr_name, 1);
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
            write_evt_zwlr_data_control_source_v1_send(dst, source, false, mime);
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
            write_evt_wl_registry_global(dst, registry, 2, ddev_name, 3);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, ddev_name, 3, manager);
            write_req_wl_data_device_manager_get_data_device(dst, manager, dev, seat);
        }),
        build_msgs(|dst| {
            write_evt_wl_data_device_data_offer(dst, dev, offer);
            write_evt_wl_data_offer_offer(dst, offer, mime);
            write_evt_wl_data_device_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_wl_data_offer_receive(dst, offer, false, mime);
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
            write_evt_zwp_primary_selection_offer_v1_offer(dst, offer, mime);
            write_evt_zwp_primary_selection_device_v1_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_zwp_primary_selection_offer_v1_receive(dst, offer, false, mime);
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
            write_evt_ext_data_control_offer_v1_offer(dst, offer, mime);
            write_evt_ext_data_control_device_v1_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_ext_data_control_offer_v1_receive(dst, offer, false, mime);
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
            write_evt_gtk_primary_selection_offer_offer(dst, offer, mime);
            write_evt_gtk_primary_selection_device_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_gtk_primary_selection_offer_receive(dst, offer, false, mime);
        }),
    ];
    let ex2_wlr: &[Vec<u8>] = &[
        build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, registry);
        }),
        build_msgs(|dst| {
            write_evt_wl_registry_global(dst, registry, 1, seat_name, 7);
            write_evt_wl_registry_global(dst, registry, 2, wlr_name, 1);
        }),
        build_msgs(|dst| {
            write_req_wl_registry_bind(dst, registry, 1, seat_name, 7, seat);
            write_req_wl_registry_bind(dst, registry, 2, wlr_name, 1, manager);
            write_req_zwlr_data_control_manager_v1_get_data_device(dst, manager, dev, seat);
        }),
        build_msgs(|dst| {
            write_evt_zwlr_data_control_device_v1_data_offer(dst, dev, offer);
            write_evt_zwlr_data_control_offer_v1_offer(dst, offer, mime);
            write_evt_zwlr_data_control_device_v1_selection(dst, dev, offer);
        }),
        build_msgs(|dst| {
            write_req_zwlr_data_control_offer_v1_receive(dst, offer, false, mime);
        }),
    ];

    let test_cases: &[&[Vec<u8>]] = &[
        ex_wl, ex_prim, ex_data, ex_gtk, ex_wlr, ex2_wl, ex2_prim, ex2_data, ex2_gtk, ex2_wlr,
    ];

    let lengths = [100_usize, 0_usize, 131073_usize]
        .iter()
        .chain(std::iter::repeat(&256_usize));
    for (test_no, (test, length)) in test_cases.iter().zip(lengths).enumerate() {
        print!("Test {}.", test_no);
        run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
        })?;
    }
    Ok(StatusOk::Pass)
}

fn proto_presentation_time(info: TestInfo) -> TestResult {
    /* Test to verify that presentation time handling does not introduce major errors */
    for (pres_clock, fast_start) in [
        (libc::CLOCK_MONOTONIC as u32, true),
        (libc::CLOCK_REALTIME as u32, false),
    ] {
        run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
                parse_wl_header(msgs.last().unwrap())
                    == (
                        feedback,
                        length_evt_wp_presentation_feedback_presented(),
                        OPCODE_WP_PRESENTATION_FEEDBACK_PRESENTED.code()
                    )
            );
            let (tv_sec_hi, tv_sec_lo, tv_nsec, _refresh, _seq_hi, _seq_lo, _flags) =
                parse_evt_wp_presentation_feedback_presented(msgs.last().unwrap()).unwrap();
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
        })?;
    }
    Ok(StatusOk::Pass)
}

fn proto_object_collision(info: TestInfo) -> TestResult {
    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
    })?;
    Ok(StatusOk::Pass)
}

fn proto_shm_buffer(info: TestInfo) -> TestResult {
    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
            write_req_wl_shm_create_pool(dst, shm, false, pool, 0);
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
                write_req_wl_shm_create_pool(dst, shm, false, pool, sz as i32);
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
    })?;
    Ok(StatusOk::Pass)
}

fn proto_shm_extend(info: TestInfo) -> TestResult {
    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
            write_req_wl_shm_create_pool(dst, shm, false, pool, 0);
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
    })?;
    Ok(StatusOk::Pass)
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
) -> BTreeMap<u32, Vec<u64>> {
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
        wayland_to_drm(WlShmFormat::R8),
        wayland_to_drm(WlShmFormat::Rgb565),
        wayland_to_drm(WlShmFormat::Argb8888),
        wayland_to_drm(WlShmFormat::Xrgb8888),
        wayland_to_drm(WlShmFormat::Xbgr16161616),
        wayland_to_drm(WlShmFormat::Abgr16161616),
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
            false,
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
    let (rmsgs, mut rfds) = ctx.comp_write(&msgs[..], &[&table_fd]).unwrap();
    rfds.reverse();

    let mut mod_table: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    let mut tranches: Vec<Vec<(u32, u64)>> = Vec::new();
    let mut current_tranche = Vec::new();
    let mut format_table: Vec<(u32, u64)> = Vec::new();
    for msg in rmsgs {
        let (obj, _len, opcode) = parse_wl_header(&msg);
        /* Have only send events to this dmabuf_feedback */
        assert!(obj == feedback);
        match MethodId::Event(opcode) {
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_MAIN_DEVICE => {
                /* ignore, Waypipe may choose a different one */
            }
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_TARGET_DEVICE => {
                /* ignore, Waypipe currently doesn't do anything interesting here */
            }
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_FLAGS => {
                /* ignore, Waypipe currently doesn't do anything interesting here */
            }
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_FORMATS => {
                let format_list =
                    parse_evt_zwp_linux_dmabuf_feedback_v1_tranche_formats(&msg).unwrap();
                /* The indices correspond to the last received format table, and should must be interpreted immediately
                 * in case the format table is changed later on */
                for c in format_list.chunks_exact(2) {
                    let idx = u16::from_le_bytes(c.try_into().unwrap());
                    let entry: (u32, u64) = *format_table
                        .get(idx as usize)
                        .expect("format index out of range");
                    current_tranche.push(entry);
                }
            }
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_TRANCHE_DONE => {
                tranches.push(current_tranche.clone());
                current_tranche = Vec::new();
            }
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_FORMAT_TABLE => {
                let fd = rfds.pop().unwrap();
                let len = parse_evt_zwp_linux_dmabuf_feedback_v1_format_table(&msg).unwrap();
                let table_contents = get_file_contents(&fd, len as usize).unwrap();
                for chunk in table_contents.chunks_exact(16) {
                    let format: u32 = u32::from_le_bytes(chunk[..4].try_into().unwrap());
                    let modifier: u64 = u64::from_le_bytes(chunk[8..].try_into().unwrap());
                    format_table.push((format, modifier));
                }
            }
            OPCODE_ZWP_LINUX_DMABUF_FEEDBACK_V1_DONE => {
                mod_table = BTreeMap::new();
                for t in &tranches {
                    for (fmt, modifier) in t {
                        mod_table
                            .entry(*fmt)
                            .and_modify(|x| x.push(*modifier))
                            .or_insert(vec![*modifier]);
                    }
                }
            }
            _ => {
                panic!("Unexpected opcode: {}", opcode);
            }
        }
    }
    assert!(rfds.is_empty());
    mod_table
}

#[cfg(feature = "dmabuf")]
fn proto_dmabuf(info: TestInfo, device: RenderDevice) -> TestResult {
    let Ok(vulk) = setup_vulkan(Some(device.id), &VideoSetting::default(), true) else {
        return Ok(StatusOk::Skipped);
    };

    run_protocol_test_with_drm_node(&info, &device, &|mut ctx: ProtocolTestContext| {
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

        let supported_modifier_table = setup_linux_dmabuf(
            &mut ctx, &vulk, display, registry, dmabuf, comp, surface, feedback,
        );
        let fmt = wayland_to_drm(WlShmFormat::Rgb565);
        let bpp = 2;
        let Some(mod_list) = supported_modifier_table.get(&fmt) else {
            println!("Skipping test, format not supported");
            return;
        };

        for (w, h, immed) in [(3, 4, true), (64, 64, true), (513, 511, false)] {
            let mut img_data = vec![0u8; w * h * bpp];
            let mut i: u16 = 0x1234;
            /* Draw a square inside */
            for y in (h / 3)..(2 * h) / 3 {
                for x in (w / 3)..(2 * w) / 3 {
                    img_data[2 * (y * w + x)..2 * (y * w + x) + 2]
                        .copy_from_slice(&i.to_le_bytes());
                    i = i.wrapping_add(1);
                }
            }

            let (img, planes) =
                vulkan_create_dmabuf(&vulk, w as u32, h as u32, fmt, mod_list, false).unwrap();

            let msgs = build_msgs(|dst| {
                write_req_zwp_linux_dmabuf_v1_create_params(dst, dmabuf, params);
                for p in planes.iter() {
                    let (mod_hi, mod_lo) = split_u64(p.modifier);
                    write_req_zwp_linux_buffer_params_v1_add(
                        dst,
                        params,
                        false,
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
    })?;
    Ok(StatusOk::Pass)
}

#[cfg(feature = "video")]
fn fill_blocks_xrgb(w: usize, h: usize) -> Vec<u8> {
    let bpp = 4;
    let colors: [u32; 4] = [0xaf0080ff, 0xbf00ff00, 0xcf800080, 0xdf808080];
    let mut img_data = vec![0u8; w * h * bpp];
    for y in 0..h {
        for x in 0..w {
            let by = (2 * y) / h;
            let bx = (2 * x) / w;
            let c = colors[2 * by + bx] ^ ((((x * y) % 2) as u32) * 0x00010101);
            img_data[bpp * (y * w + x)..bpp * (y * w + x) + bpp].copy_from_slice(&c.to_le_bytes());
        }
    }
    img_data
}

#[cfg(feature = "video")]
fn fill_pseudorand_xrgb(w: usize, h: usize) -> Vec<u8> {
    let bpp = 4;
    let mut img_data = vec![0u8; w * h * bpp];
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
    modifier_list: &[u64],
    initial_data: &[u8],
) -> Result<(Arc<VulkanDmabuf>, Arc<VulkanDmabuf>), String> {
    let (img, planes) =
        vulkan_create_dmabuf(vulk, w as u32, h as u32, fmt, modifier_list, false).unwrap();

    /* Initialize the image with garbage data */
    let tmp_img = Arc::new(vulkan_get_buffer(vulk, img.nominal_size(None), false).unwrap());
    copy_onto_dmabuf(&img, &tmp_img, initial_data).unwrap();

    let msgs = build_msgs(|dst| {
        write_req_zwp_linux_dmabuf_v1_create_params(dst, dmabuf, params);
        for p in planes.iter() {
            let (mod_hi, mod_lo) = split_u64(p.modifier);
            write_req_zwp_linux_buffer_params_v1_add(
                dst,
                params,
                false,
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

    let mirror = vulkan_import_dmabuf(vulk, planes, w as u32, h as u32, fmt, false).unwrap();
    Ok((img, mirror))
}

#[cfg(feature = "video")]
fn test_dmavid_inner(vulk: &Arc<Vulkan>, info: &TestInfo, opts: &WaypipeOptions) -> bool {
    let accurate_replication = AtomicBool::new(true);
    run_protocol_test_with_opts(
        info, opts, opts,
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

            let supported_modifier_table = setup_linux_dmabuf(
                &mut ctx, vulk, display, registry, dmabuf, comp, surface, feedback,
            );
            let fmt = wayland_to_drm(WlShmFormat::Xrgb8888);
            let Some(modifier_list) = supported_modifier_table.get(&fmt) else {
                println!("Skipping test, format {:#08x} not supported", fmt);
                return;
            };
            // todo: run these in parallel?

            // The small (width <= 32, height <= 16 test sizes fail) with uniform (88)
            // green channel. height=16 shows nonuniform output but still has high error
            for (w, h) in [(64, 64), (257, 240), (1, 1), (11, 200), (201, 10)] {
                println!("Testing image transfer for WxH: {}x{}", w, h);

                let seed = fill_pseudorand_xrgb(w, h);
                let (img, mirror) = create_dmabuf_and_copy(
                    vulk,
                    &mut ctx,
                    params,
                    dmabuf,
                    buffer,
                    w,
                    h,
                    fmt,
                    modifier_list,
                    &seed,
                )
                .unwrap();

                let img_data = fill_blocks_xrgb(w, h);
                let tmp_img =
                    Arc::new(vulkan_get_buffer(vulk, img.nominal_size(None), true).unwrap());
                copy_onto_dmabuf(&img, &tmp_img, &img_data).unwrap();

                ctx.prog_write_passthrough(build_msgs(|dst| {
                    write_req_wl_surface_attach(dst, surface, buffer, 0, 0);
                    write_req_wl_surface_damage_buffer(dst, surface, 0, 0, i32::MAX, i32::MAX);
                    write_req_wl_surface_commit(dst, surface);
                }));

                let tmp_mirror =
                    Arc::new(vulkan_get_buffer(vulk, mirror.nominal_size(None), true).unwrap());
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
                        /* XRGB8888 bytes are ordered: B G R x */
                        for (i, channel) in ["Blue", "Green", "Red"].iter().enumerate() {
                            println!("{} channel, original / replicated", channel);
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
                                    print!("{:2x}/{:2x} ", base[i], rep[i]);
                                }
                                println!();
                            }
                        }
                    }
                }

                if avg_diff > threshold {
                    accurate_replication.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                // TODO: once video library bugs are resolved, set this
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
        }
    ).unwrap();

    accurate_replication.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(feature = "video")]
fn test_video_combo(
    vulk: &Arc<Vulkan>,
    info: &TestInfo,
    device: &RenderDevice,
    video_format: VideoFormat,
    try_hw_dec: bool,
    try_hw_enc: bool,
    accurate_video_replication: bool,
) -> bool {
    let opts = WaypipeOptions {
        compression: Compression::None,
        video: VideoSetting {
            format: Some(video_format),
            bits_per_frame: None, // note: very high/low values can cause codec failure
            enc_pref: Some(if try_hw_enc {
                CodecPreference::HW
            } else {
                CodecPreference::SW
            }),
            dec_pref: Some(if try_hw_dec {
                CodecPreference::HW
            } else {
                CodecPreference::SW
            }),
        },
        title_prefix: "",
        drm_node: Some(device.id),
        wire_version: None,
    };
    println!(
        "\nTrying combination: video={:?}, try_hw_dec={}, try_hw_enc={}",
        video_format, try_hw_dec, try_hw_enc
    );
    let pass = test_dmavid_inner(vulk, info, &opts);
    println!(
        "Result for video={:?}, try_hw_dec={}, try_hw_enc={}: {}",
        video_format,
        try_hw_dec,
        try_hw_enc,
        if pass { "pass" } else { "fail" }
    );

    // NOTE: as of writing, AMD hardware video decoding, and Intel hardware video
    // encoding, of size w<=32, h<=16 (w=32,h=16) images does not accurately
    // reproduce colors
    if accurate_video_replication {
        assert!(pass);
    }
    pass
}

#[cfg(feature = "video")]
fn proto_dmavid_vp9(info: TestInfo, device: RenderDevice) -> TestResult {
    let Ok(vulk) = setup_vulkan(Some(device.id), &VideoSetting::default(), true) else {
        return Ok(StatusOk::Pass);
    };
    if test_video_combo(&vulk, &info, &device, VideoFormat::VP9, false, false, false) {
        Ok(StatusOk::Pass)
    } else {
        Err(StatusBad::Unclear("Video replication not exact".into()))
    }
}

#[cfg(feature = "video")]
fn proto_dmavid_h264(info: TestInfo, device: RenderDevice) -> TestResult {
    let Ok(vulk) = setup_vulkan(Some(device.id), &VideoSetting::default(), true) else {
        return Ok(StatusOk::Pass);
    };

    /* Test all hardware encoding/decoding combinations to sure formats are compatible */
    let mut all_pass = true;
    for (try_hw_dec, try_hw_enc, accurate_video_replication) in [
        (false, false, true),
        (true, false, false),
        (true, true, false),
        (false, true, false),
    ] {
        if !test_video_combo(
            &vulk,
            &info,
            &device,
            VideoFormat::H264,
            try_hw_dec,
            try_hw_enc,
            accurate_video_replication,
        ) {
            all_pass = false;
        }
    }
    if all_pass {
        Ok(StatusOk::Pass)
    } else {
        Err(StatusBad::Unclear("Video replication not exact".into()))
    }
}

fn proto_oversized(info: TestInfo) -> TestResult {
    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
    })?;
    Ok(StatusOk::Pass)
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

fn proto_shm_damage(info: TestInfo) -> TestResult {
    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
                write_req_wl_shm_create_pool(dst, shm, false, pool, file_sz as i32);
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
    })?;
    Ok(StatusOk::Pass)
}

#[cfg(feature = "dmabuf")]
fn proto_dmabuf_damage(info: TestInfo, device: RenderDevice) -> TestResult {
    let Ok(vulk) = setup_vulkan(Some(device.id), &VideoSetting::default(), true) else {
        return Ok(StatusOk::Pass);
    };

    run_protocol_test_with_drm_node(&info, &device, &|mut ctx: ProtocolTestContext| {
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

        let supported_modifier_table = setup_linux_dmabuf(
            &mut ctx, &vulk, display, registry, dmabuf, comp, surface, feedback,
        );

        /* For dmabuf replication, the format (well, texel size) affects diff alignment */
        for (w, h) in [(64_usize, 64_usize), (257_usize, 257_usize)] {
            for wl_format in [
                WlShmFormat::R8,
                WlShmFormat::Rgb565,
                WlShmFormat::Argb8888,
                WlShmFormat::Abgr16161616,
            ] {
                let bpp = match wl_format {
                    WlShmFormat::Abgr16161616 => 8,
                    WlShmFormat::Argb8888 => 4,
                    WlShmFormat::Rgb565 => 2,
                    WlShmFormat::R8 => 1,
                    _ => unreachable!(),
                };
                let format = wayland_to_drm(wl_format);
                let stride = bpp * w;
                let file_sz = h * stride;

                let Some(modifier_list) = supported_modifier_table.get(&format) else {
                    println!("Skipping test, format {:#08x} not supported", format);
                    continue;
                };

                let mut base = vec![0x80; file_sz];

                let (img, mirror) = create_dmabuf_and_copy(
                    &vulk,
                    &mut ctx,
                    params,
                    dmabuf,
                    buffer,
                    w,
                    h,
                    format,
                    modifier_list,
                    &base,
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

                let dup = copy_from_dmabuf(&mirror, &tmp_rd).unwrap();
                assert!(
                    dup == base,
                    "initial mismatch {} {}\n{:?}\n{:?}",
                    dup.len(),
                    base.len(),
                    dup,
                    base
                );

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
                        "mismatch iter {}, {} {}\n{:?}\n{:?}",
                        iter,
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
    })?;
    Ok(StatusOk::Pass)
}

#[cfg(feature = "dmabuf")]
fn proto_explicit_sync(info: TestInfo, device: RenderDevice) -> TestResult {
    let Ok(vulk) = setup_vulkan(Some(device.id), &VideoSetting::default(), true) else {
        return Ok(StatusOk::Pass);
    };

    run_protocol_test_with_drm_node(&info, &device, &|mut ctx: ProtocolTestContext| {
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

        let supported_modifier_table = setup_linux_dmabuf(
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
            write_req_wp_linux_drm_syncobj_manager_v1_import_timeline(
                dst, manager, false, timeline,
            );
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

        let Some(modifier_list) = supported_modifier_table.get(&format) else {
            println!("Skipping test, format {:#08x} not supported", format);
            return;
        };
        let (img, mirror) = create_dmabuf_and_copy(
            &vulk,
            &mut ctx,
            params,
            dmabuf,
            buffer,
            w,
            h,
            format,
            modifier_list,
            &base,
        )
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
    })?;

    Ok(StatusOk::Pass)
}

fn proto_many_fds(info: TestInfo) -> TestResult {
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

    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
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
                write_evt_wl_keyboard_keymap(dst, keyboard, false, 1, contents.len() as u32);
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
    })?;
    Ok(StatusOk::Pass)
}

fn proto_title_prefix(info: TestInfo) -> TestResult {
    // /* test lengths are: empty, short, too long for small buffers, cannot fit in a Wayland message */
    for (prefix_len, fail) in [(0, false), (100, false), (10000, true), (100000, true)] {
        let prefix = String::from("a").repeat(prefix_len);
        let options = WaypipeOptions {
            wire_version: None,
            drm_node: None,
            video: VideoSetting::default(),
            title_prefix: &prefix,
            compression: Compression::None,
        };
        run_protocol_test_with_opts(
            &info,
            &options,
            &options,
            &|mut ctx: ProtocolTestContext| {
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
                        let title = vec![b'b'; title_len];
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
                        for (title_len, msg) in title_lengths.iter().zip(rmsgs.iter()) {
                            assert!(
                                parse_wl_header(msg)
                                    == (toplevel, msg.len(), OPCODE_XDG_TOPLEVEL_SET_TITLE.code())
                            );
                            let title = parse_req_xdg_toplevel_set_title(msg).unwrap();
                            let orig_title = vec![b'b'; *title_len];
                            /* The title prefix is added twice, once per main loop instance */
                            let mut ref_title = vec![b'a'; prefix_len * 2];
                            ref_title.extend_from_slice(&orig_title);
                            assert!(title == ref_title);
                        }
                    }
                }
            },
        )?;
    }
    Ok(StatusOk::Pass)
}

/* Test that basic wlr_screencopy operations work with wl_shm buffers */
fn proto_screencopy_shm(info: TestInfo) -> TestResult {
    run_protocol_test(&info, &|mut ctx: ProtocolTestContext| {
        let [display, reg, shm, screencopy, output, shm_pool, buffer1, buffer2, frame, ..] =
            ID_SEQUENCE;
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, reg);
        }));
        let scrcopy_name = "zwlr_screencopy_manager_v1".as_bytes();
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_registry_global(dst, reg, 1, "wl_shm".as_bytes(), 2);
            write_evt_wl_registry_global(dst, reg, 2, scrcopy_name, 3);
            write_evt_wl_registry_global(dst, reg, 3, "wl_output".as_bytes(), 4);
        }));
        let (w, h, fmt) = (33, 17, WlShmFormat::Xrgb8888 as u32);
        let pool_sz: usize = 3 * (w * h * 4) / 2;
        let offset1: usize = 10;
        let offset2: usize = pool_sz - (w * h * 4);
        let seed_contents: Vec<u8> = (0..pool_sz).map(|x| (x * 101) as u8).collect();
        let mut copy_contents: Vec<u8> = seed_contents.clone();
        assert!(offset1 <= 333 && 666 <= offset1 + w * h * 4);
        copy_contents[333..666].fill(0xaa);
        let shm_fd = make_file_with_contents(&seed_contents).unwrap();

        let setup = build_msgs(|dst| {
            write_req_wl_registry_bind(dst, reg, 1, "wl_shm".as_bytes(), 2, shm);
            write_req_wl_registry_bind(dst, reg, 2, scrcopy_name, 3, screencopy);
            write_req_wl_registry_bind(dst, reg, 3, "wl_output".as_bytes(), 4, output);
            write_req_wl_shm_create_pool(dst, shm, false, shm_pool, pool_sz.try_into().unwrap());
            write_req_wl_shm_pool_create_buffer(
                dst,
                shm_pool,
                buffer1,
                offset1.try_into().unwrap(),
                w.try_into().unwrap(),
                h.try_into().unwrap(),
                (w * 4).try_into().unwrap(),
                fmt,
            );
            write_req_wl_shm_pool_create_buffer(
                dst,
                shm_pool,
                buffer2,
                offset2.try_into().unwrap(),
                w.try_into().unwrap(),
                h.try_into().unwrap(),
                (w * 4).try_into().unwrap(),
                fmt,
            );
            write_req_zwlr_screencopy_manager_v1_capture_output_region(
                dst,
                screencopy,
                frame,
                0,
                output,
                1,
                1,
                (w - 2) as _,
                (h - 2) as _,
            );
        });
        let fds = [&shm_fd];
        let (rmsgs, mut rfds) = ctx.prog_write(&setup, &fds).unwrap();
        assert!(rmsgs.concat() == setup);
        assert!(rfds.len() == 1);
        let rfd = rfds.pop().unwrap();
        update_file_contents(&rfd, &copy_contents).unwrap();

        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_zwlr_screencopy_frame_v1_buffer(
                dst,
                frame,
                fmt,
                w as _,
                h as _,
                (4 * w) as _,
            );
            write_evt_zwlr_screencopy_frame_v1_buffer_done(dst, frame);
        }));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_zwlr_screencopy_frame_v1_copy(dst, frame, buffer2);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_zwlr_screencopy_frame_v1_failed(dst, frame);
        }));
        /* Check that the failed screencopy does not lead to replication
         * (Although technically Waypipe _could_ eagerly make updates in this scenario,
         * since the buffer contents were already updated, doing so would be inefficient;
         * the wlr-screencopy protocol also says nothing about the buffer state on failure.)
         */
        let check1 = get_file_contents(&shm_fd, pool_sz).unwrap();
        assert!(check1 == seed_contents);

        // Check diff
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_zwlr_screencopy_frame_v1_destroy(dst, frame);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_display_delete_id(dst, display, frame.0);
        }));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_zwlr_screencopy_manager_v1_capture_output(dst, screencopy, frame, 0, output);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_zwlr_screencopy_frame_v1_buffer(
                dst,
                frame,
                fmt,
                w as _,
                h as _,
                (4 * w) as _,
            );
            write_evt_zwlr_screencopy_frame_v1_buffer_done(dst, frame);
        }));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_zwlr_screencopy_frame_v1_copy(dst, frame, buffer1);
        }));
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_zwlr_screencopy_frame_v1_ready(dst, frame, 0, 1, 123456789);
        }));
        /* Check that the update is replicated */
        let check2 = get_file_contents(&shm_fd, pool_sz).unwrap();
        assert!(check2 == copy_contents);
    })?;
    Ok(StatusOk::Pass)
}

/* Test that basic wlr_screencopy operations work with dmabufs */
#[cfg(feature = "dmabuf")]
fn proto_screencopy_dmabuf(info: TestInfo, device: RenderDevice) -> TestResult {
    let Ok(vulk) = setup_vulkan(Some(device.id), &VideoSetting::default(), true) else {
        return Ok(StatusOk::Skipped);
    };

    run_protocol_test_with_drm_node(&info, &device, &|mut ctx: ProtocolTestContext| {
        let [display, reg, dmabuf, screencopy, output, frame, params, buffer, ..] = ID_SEQUENCE;

        let fmt = wayland_to_drm(WlShmFormat::R8);
        let bpp = 1;
        let (w, h) = (8, 8);
        let img_size = (w * h) as usize * bpp;
        let img_data = vec![0x33u8; img_size];
        let mod_data = vec![0x44u8; img_size];
        let modifier_list = vulk.get_supported_modifiers(fmt);

        let copy_buf = Arc::new(vulkan_get_buffer(&vulk, img_size, true).unwrap());

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_display_get_registry(dst, display, reg);
        }));
        let scrcopy_name = "zwlr_screencopy_manager_v1".as_bytes();
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_wl_registry_global(dst, reg, 1, "zwp_linux_dmabuf_v1".as_bytes(), 3);
            write_evt_wl_registry_global(dst, reg, 2, scrcopy_name, 3);
            write_evt_wl_registry_global(dst, reg, 3, "wl_output".as_bytes(), 4);
        }));
        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_wl_registry_bind(dst, reg, 1, "zwp_linux_dmabuf_v1".as_bytes(), 3, dmabuf);
            write_req_wl_registry_bind(dst, reg, 2, scrcopy_name, 3, screencopy);
            write_req_wl_registry_bind(dst, reg, 3, "wl_output".as_bytes(), 4, output);
            write_req_zwlr_screencopy_manager_v1_capture_output(dst, screencopy, frame, 0, output);
        }));

        /* Note: This assumes the Waypipe instance supports all the modifiers that the
         * test framework does */
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_zwp_linux_dmabuf_v1_format(dst, dmabuf, fmt);
            for m in &modifier_list {
                let (mod_hi, mod_lo) = split_u64(*m);
                write_evt_zwp_linux_dmabuf_v1_modifier(dst, dmabuf, fmt, mod_hi, mod_lo);
            }
            write_evt_zwlr_screencopy_frame_v1_linux_dmabuf(dst, frame, fmt, w, h);
            write_evt_zwlr_screencopy_frame_v1_buffer_done(dst, frame);
        }));

        let (prog_img, comp_img) = create_dmabuf_and_copy(
            &vulk,
            &mut ctx,
            params,
            dmabuf,
            buffer,
            w as _,
            h as _,
            fmt,
            &modifier_list,
            &img_data,
        )
        .unwrap();

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_zwlr_screencopy_frame_v1_copy(dst, frame, buffer);
        }));

        copy_onto_dmabuf(&comp_img, &copy_buf, &mod_data).unwrap();
        ctx.comp_write_passthrough(build_msgs(|dst| {
            write_evt_zwlr_screencopy_frame_v1_ready(dst, frame, 0, 1, 123456789);
        }));

        let output = copy_from_dmabuf(&prog_img, &copy_buf).unwrap();
        assert!(output == mod_data);

        ctx.prog_write_passthrough(build_msgs(|dst| {
            write_req_zwlr_screencopy_frame_v1_destroy(dst, frame);
        }));
    })?;
    Ok(StatusOk::Pass)
}

fn main() -> ExitCode {
    let command = ClapCommand::new(env!("CARGO_BIN_NAME"))
        .help_expected(true)
        .flatten_help(false)
        .about(
            "A collection of protocol tests to be run against the Waypipe client\n\
        and server connection subprocesses. This is not expected to be packaged\n\
        and has no stability guarantee.",
        )
        .next_line_help(false)
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("client-path")
                .help("waypipe binary to use as client")
                .required(true)
                .value_parser(value_parser!(OsString)),
        )
        .arg(
            Arg::new("server-path")
                .help("waypipe binary to use as server")
                .required(true)
                .value_parser(value_parser!(OsString)),
        )
        .arg(
            Arg::new("test-filter")
                .help("If present, run tests whose names contain any test filter string")
                .num_args(0..)
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("list")
                .long("list")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["client-path", "server-path", "test-filter"])
                .help("List available tests"),
        )
        .arg(
            Arg::new("quiet")
                .long("quiet")
                .short('q')
                .action(ArgAction::SetTrue)
                .help("Hide captured output"),
        );

    let matches = command.get_matches();

    let f: Vec<&str> = matches
        .get_many::<String>("test-filter")
        .map(|x| x.map(|y| y.as_str()).collect())
        .unwrap_or_default();

    let vk_device_ids = list_vulkan_device_ids();

    /* Construct a list of all tests */
    let mut tests: Vec<(String, Box<dyn Fn(TestInfo) -> TestResult>)> = Vec::new();

    register_single(&mut tests, &f, "basic", proto_basic);
    register_single(&mut tests, &f, "base_wire", proto_base_wire);
    register_single(&mut tests, &f, "keymap", proto_keymap);
    register_single(&mut tests, &f, "many_fds", proto_many_fds);
    register_single(&mut tests, &f, "object_collision", proto_object_collision);
    register_single(&mut tests, &f, "oversized", proto_oversized);
    register_single(&mut tests, &f, "pipe_write", proto_pipe_write);
    register_single(&mut tests, &f, "presentation_time", proto_presentation_time);
    register_single(&mut tests, &f, "screencopy_shm", proto_screencopy_shm);
    register_single(&mut tests, &f, "shm_buffer", proto_shm_buffer);
    register_single(&mut tests, &f, "shm_damage", proto_shm_damage);
    register_single(&mut tests, &f, "shm_extend", proto_shm_extend);
    register_single(&mut tests, &f, "title_prefix", proto_title_prefix);

    #[cfg(feature = "dmabuf")]
    {
        register_per_device(&mut tests, &f, &vk_device_ids, "dmabuf", proto_dmabuf);
        register_per_device(
            &mut tests,
            &f,
            &vk_device_ids,
            "dmabuf_damage",
            proto_dmabuf_damage,
        );
        register_per_device(
            &mut tests,
            &f,
            &vk_device_ids,
            "explicit_sync",
            proto_explicit_sync,
        );
        register_per_device(
            &mut tests,
            &f,
            &vk_device_ids,
            "screencopy_dmabuf",
            proto_screencopy_dmabuf,
        );
    }

    #[cfg(feature = "video")]
    {
        register_per_device(
            &mut tests,
            &f,
            &vk_device_ids,
            "dmavid_h264",
            proto_dmavid_h264,
        );
        register_per_device(
            &mut tests,
            &f,
            &vk_device_ids,
            "dmavid_vp9",
            proto_dmavid_vp9,
        );
    }

    if matches.get_flag("list") {
        for (name, _) in tests {
            println!("{}", name);
        }
        return ExitCode::SUCCESS;
    }

    let client_file: &OsString = matches.get_one("client-path").unwrap();
    let server_file: &OsString = matches.get_one("server-path").unwrap();
    let quiet: bool = matches.get_flag("quiet");

    let mut mask = signal::SigSet::empty();
    mask.add(signal::SIGCHLD);
    let mut pollmask = mask
        .thread_swap_mask(signal::SigmaskHow::SIG_BLOCK)
        .map_err(|x| tag!("Failed to set sigmask: {}", x))
        .unwrap();
    pollmask.remove(signal::SIGCHLD);

    let sigaction = signal::SigAction::new(
        signal::SigHandler::Handler(noop_signal_handler),
        signal::SaFlags::SA_NOCLDSTOP,
        signal::SigSet::empty(),
    );
    unsafe {
        // SAFETY: signal handler installed is trivial and replaces nothing
        signal::sigaction(signal::Signal::SIGCHLD, &sigaction)
            .map_err(|x| tag!("Failed to set sigaction: {}", x))
            .unwrap();
    }

    let mut nfail: usize = 0;
    let mut nskip: usize = 0;
    for (name, func) in &tests {
        print!("{} ...", name);

        let info = TestInfo {
            test_name: name,
            waypipe_client: client_file,
            waypipe_server: server_file,
        };

        let (read_out, new_stdouterr) = unistd::pipe2(fcntl::OFlag::empty()).unwrap();
        let new_stdin = unsafe {
            /* SAFETY: newly opened file descriptor */
            OwnedFd::from_raw_fd(
                fcntl::open(
                    "/dev/null",
                    fcntl::OFlag::O_RDONLY,
                    nix::sys::stat::Mode::empty(),
                )
                .unwrap(),
            )
        };

        let res = match unsafe {
            /* SAFETY: this program is not multi-threaded at this point.
             * (No vulkan setup or initialization has been done.) */
            unistd::fork().unwrap()
        } {
            unistd::ForkResult::Child => {
                /* Blocking wait for child to complete */
                #[allow(unused_unsafe)] /* dup2 is file-descriptor-unsafe */
                unsafe {
                    /* Atomically replace STDOUT, STDERR, STDIN; this may break library code which
                     * incorrectly assumes standard io file descriptors never change properties
                     * (e.g., by caching isatty()). */
                    unistd::dup2(new_stdouterr.as_raw_fd(), libc::STDOUT_FILENO).unwrap();
                    unistd::dup2(new_stdouterr.as_raw_fd(), libc::STDERR_FILENO).unwrap();
                    unistd::dup2(new_stdin.as_raw_fd(), libc::STDIN_FILENO).unwrap();
                }
                drop(read_out);

                // TODO: add option to disable this if nontrivial errors ever need to
                // be debugged.

                /* Disable core dumps for child process, because the tests report errors
                 * using using panic! or a failed assert!, and core dumps should not need
                 * recording. However: this _also_ prevents the Waypipe instances from
                 * making a core dump */
                assert!(
                    unsafe {
                        let x = libc::rlimit {
                            rlim_cur: 0,
                            rlim_max: 0,
                        };
                        /* SAFETY: x is properly aligned, is not captured by setrlimit, and lives until end of scope */
                        libc::setrlimit(libc::RLIMIT_CORE, &x)
                    } != -1
                );

                // TODO: process group configuration to kill the child process and
                // everything it spawns on timeout events?

                let ret = func(info);
                return match ret {
                    Ok(StatusOk::Pass) => ExitCode::SUCCESS,
                    Ok(StatusOk::Skipped) => ExitCode::from(EXITCODE_SKIPPED),
                    Err(StatusBad::Fail(msg)) => {
                        println!("Test {} failed with error: {}", name, msg);
                        ExitCode::FAILURE
                    }
                    Err(StatusBad::Unclear(msg)) => {
                        println!("Test {} unclear, with error: {}", name, msg);
                        ExitCode::from(EXITCODE_UNCLEAR)
                    }
                };
            }
            unistd::ForkResult::Parent { child } => {
                drop(new_stdin);
                drop(new_stdouterr);

                set_nonblock(&read_out).unwrap();

                /* Wait for the child to complete, and capture its output */
                let mut log = Vec::new();
                let mut tmp = vec![0u8; 1 << 18];
                let child_status: WaitStatus;
                loop {
                    let status = wait::waitpid(child, Some(wait::WaitPidFlag::WNOHANG)).unwrap();
                    match status {
                        wait::WaitStatus::Exited(..) | wait::WaitStatus::Signaled(..) => {
                            child_status = status;
                            break;
                        }
                        _ => (),
                    }

                    let mut pfds = [poll::PollFd::new(read_out.as_fd(), poll::PollFlags::POLLIN)];
                    let res = poll::ppoll(&mut pfds, None, Some(pollmask));
                    if let Err(errno) = res {
                        assert!(errno == Errno::EINTR || errno == Errno::EAGAIN);
                        continue;
                    }

                    let rev = pfds[0].revents().unwrap();
                    if rev.contains(poll::PollFlags::POLLERR)
                        || rev.contains(poll::PollFlags::POLLHUP)
                    {
                        /* Child closed connection (or more likely, died).
                         * Data remaining in pipe will be read. */
                        child_status = wait::waitpid(child, None).unwrap();
                        break;
                    }

                    if !rev.contains(poll::PollFlags::POLLIN) {
                        continue;
                    }

                    let eof_or_err = match unistd::read(read_out.as_raw_fd(), &mut tmp) {
                        Ok(n) => {
                            if n == 0 {
                                true
                            } else {
                                log.extend_from_slice(&tmp[..n]);
                                false
                            }
                        }
                        Err(Errno::EAGAIN) => false,
                        Err(Errno::EINTR) => false,
                        Err(_) => true,
                    };
                    if eof_or_err {
                        child_status = wait::waitpid(child, None).unwrap();
                        break;
                    }
                }

                /* Read all remaining data in the pipe, dropping anything
                 * that was read after receipt of the information of test process death */
                loop {
                    match unistd::read(read_out.as_raw_fd(), &mut tmp) {
                        Ok(n) => {
                            if n == 0 {
                                break;
                            }
                            log.extend_from_slice(&tmp[..n]);
                        }
                        Err(Errno::EINTR) => continue,
                        Err(_) => break,
                    }
                }

                if !quiet {
                    println!("captured output:");
                    println!("{}", String::from_utf8_lossy(&log));
                }

                match child_status {
                    wait::WaitStatus::Exited(pid, exit_code) => {
                        assert!(pid == child);
                        let e = if exit_code > (u8::MAX as i32) || exit_code < 0 {
                            u8::MAX
                        } else {
                            exit_code as u8
                        };
                        match e {
                            0 => TestCategory::Pass,
                            EXITCODE_SKIPPED => TestCategory::Skipped,
                            EXITCODE_UNCLEAR => TestCategory::Unclear,
                            _ => TestCategory::Fail,
                        }
                    }
                    wait::WaitStatus::Signaled(pid, signal, dump) => {
                        if false {
                            println!(
                                "Test process {} crashed with signal={}; coredump={}",
                                pid, signal, dump
                            );
                        }
                        /* The test process aborting signals either the Waypipe instance
                         * failing (making the test panic) or a major bug in the test. */
                        TestCategory::Fail
                    }
                    _ => {
                        todo!("Unexpected exit status");
                    }
                }
            }
        };

        let s = match res {
            TestCategory::Pass => "ok",
            TestCategory::Fail => {
                nfail += 1;
                "FAILED"
            }
            TestCategory::Skipped => {
                nskip += 1;
                "skipped"
            }
            TestCategory::Unclear => "UNCLEAR",
        };

        println!(" {}", s);
    }

    if nfail > 0 {
        ExitCode::FAILURE
    } else if nskip == tests.len() {
        ExitCode::from(EXITCODE_SKIPPED)
    } else {
        ExitCode::SUCCESS
    }
}
