/* SPDX-License-Identifier: GPL-3.0-or-later */
/*! Protocol tests using the `test_proto` test runner */
#![cfg(feature = "test_proto")]
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, ExitCode, Stdio};

/** Run the named test, print its output, and return an appropriate exit code */
fn do_test(name: &str) -> ExitCode {
    let waypipe_bin = env!("CARGO_BIN_EXE_waypipe");
    let test_proto_bin = env!("CARGO_BIN_EXE_test_proto");

    let cross_runner = std::env::var_os("CROSS_TARGET_RUNNER");
    let mut args: Vec<&OsStr> = Vec::new();
    let cx: &OsStr = cross_runner
        .as_ref()
        .map(|x| x.as_os_str())
        .unwrap_or(OsStr::new(""));
    /* note: CROSS_TARGET_RUNNER is typically something like 'CROSS_TARGET_RUNNER=/linux-runner aarch64' */
    for chunk in cx.as_encoded_bytes().split(|x| *x == b' ') {
        if !chunk.is_empty() {
            args.push(&OsStr::from_bytes(chunk));
        }
    }
    args.push(&OsStr::new(test_proto_bin));
    args.push(&OsStr::new(waypipe_bin));
    args.push(&OsStr::new(waypipe_bin));
    args.push(&OsStr::new(name));

    let mut command = Command::new(args[0]);
    command
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = match command.spawn() {
        Err(x) => {
            println!("Error when running test_proto: {:?}", x);
            return ExitCode::FAILURE;
        }
        Ok(c) => c,
    };

    let output = match child.wait_with_output() {
        Err(x) => {
            println!("Error when waiting for test_proto to complete: {:?}", x);
            return ExitCode::FAILURE;
        }
        Ok(output) => output,
    };
    /* Write with println! so Rust's test framework properly captures this */
    println!("Test stdout ({} bytes):", output.stdout.len());
    println!("{}", String::from_utf8_lossy(&output.stdout));
    /* test_proto is not expected to write anything to stderr; print it just in case */
    println!("Test stderr ({} bytes):", output.stderr.len());
    println!("{}", String::from_utf8_lossy(&output.stderr));

    match output.status.code() {
        Some(0) => {
            println!("Test {} passed.", name);
            ExitCode::SUCCESS
        }
        Some(77) => {
            println!("Test {} was skipped.", name);
            ExitCode::SUCCESS
        }
        Some(x) => {
            println!("Test {} failed (exit code {}).", name, x);
            ExitCode::FAILURE
        }
        None => {
            println!("Test {} failed (no exit code).", name);
            ExitCode::FAILURE
        }
    }
}

macro_rules! define_test {
    ($x:ident) => {
        #[test]
        fn $x() -> ExitCode {
            do_test(stringify!($x))
        }
    };
}

mod proto {
    use crate::{do_test, ExitCode};

    define_test! {basic}
    define_test! {base_wire}
    define_test! {gamma_control}
    define_test! {keymap}
    define_test! {many_fds}
    define_test! {object_collision}
    define_test! {oversized}
    define_test! {pipe_write}
    define_test! {presentation_time}
    define_test! {screencopy_shm_ext}
    define_test! {screencopy_shm_wlr}
    define_test! {shm_buffer}
    define_test! {shm_damage}
    define_test! {shm_extend}
    define_test! {title_prefix}
    #[cfg(feature = "dmabuf")]
    define_test! {dmabuf}
    #[cfg(feature = "dmabuf")]
    define_test! {dmabuf_damage}
    #[cfg(feature = "video")]
    define_test! {dmavid_h264}
    #[cfg(feature = "video")]
    define_test! {dmavid_vp9}
    #[cfg(feature = "dmabuf")]
    define_test! {explicit_sync}
    #[cfg(feature = "dmabuf")]
    define_test! {screencopy_dmabuf_ext}
    #[cfg(feature = "dmabuf")]
    define_test! {screencopy_dmabuf_wlr}
}
