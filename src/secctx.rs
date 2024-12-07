/* SPDX-License-Identifier: GPL-3.0-or-later */

use crate::tag;
use crate::util::*;
use crate::wayland::*;
use crate::wayland_gen::*;
use log::debug;
use nix::{sys::socket, unistd};
use std::io::{Cursor, IoSlice, Write};
use std::os::fd::{AsRawFd, OwnedFd};

fn read_event(connection: &OwnedFd) -> Result<Vec<u8>, String> {
    let mut msg = vec![0; 8];
    match unistd::read(connection.as_raw_fd(), &mut msg) {
        Err(x) => {
            return Err(tag!("Reading from compositor failed: {:?}", x));
        }
        Ok(s) => {
            if s < 8 {
                return Err(tag!("Incomplete event read: only {} of 8 bytes", s));
            }
        }
    }

    let (_object_id, length, _opcode) = parse_wl_header(&msg);
    msg.resize(length, 0);

    match unistd::read(connection.as_raw_fd(), &mut msg[8..]) {
        Err(x) => {
            return Err(tag!("Reading from compositor failed: {:?}", x));
        }
        Ok(s) => {
            if s < msg.len() - 8 {
                return Err(tag!(
                    "Incomplete event read: only {} of {} bytes",
                    s + 8,
                    msg.len()
                ));
            }
        }
    }
    Ok(msg)
}

/* `connection` is a blocking socket connecting to the compositor.
 * Returns the ''
 */
pub fn provide_secctx(
    connection: OwnedFd,
    app_id: &str,
    listen_fd: OwnedFd,
    close_fd: OwnedFd,
) -> Result<(), String> {
    let mut tmp = [0_u8; 64];
    let tmp_len = tmp.len();
    let mut dst = &mut tmp[..];
    let (display, registry, callback, manager, context, callback2) =
        (ObjId(1), ObjId(2), ObjId(3), ObjId(4), ObjId(5), ObjId(6));

    write_req_wl_display_get_registry(&mut dst, display, registry);
    write_req_wl_display_sync(&mut dst, display, callback);
    let msg_len = tmp_len - dst.len();
    let msg = &tmp[..msg_len];

    debug!("Requesting compositor globals");
    match unistd::write(&connection, msg) {
        Err(x) => {
            return Err(tag!("Failed to write to compositor: {:?}", x));
        }
        Ok(s) => {
            assert!(s == msg.len());
        }
    }

    let sec_man_name: &[u8] = INTERFACE_TABLE
        [WaylandInterface::WpSecurityContextManagerV1 as usize]
        .name
        .as_bytes();
    let secctx_name: u32 = loop {
        let msg = read_event(&connection)?;
        let (object_id, _length, opcode) = parse_wl_header(&msg);

        if object_id == callback {
            /* wl_callback; only event is 'done' */
            return Err(tag!(
                "Compositor did not provide wp_security_context_manager_v1 global"
            ));
        } else if object_id == registry {
            if opcode != OPCODE_WL_REGISTRY_GLOBAL.code() {
                // global remove should not happen in first roundtrip for reasonable compositors
                debug!("Unexpected event from registry {}: {}", object_id, opcode);
                continue;
            }
            let (name, interface, version) = parse_evt_wl_registry_global(&msg[..])?;
            if interface != sec_man_name {
                continue;
            }
            assert!(version >= 1);
            break name;
        } else {
            debug!("Unexpected event from object {}: {}", object_id, opcode);
        }
    };

    let mut tmp = [0_u8; 512];
    let tmp_len = tmp.len();
    let mut dst = &mut tmp[..];
    write_req_wl_registry_bind(&mut dst, registry, secctx_name, sec_man_name, 1, manager);
    write_req_wp_security_context_manager_v1_create_listener(&mut dst, manager, false, context);
    write_req_wp_security_context_v1_set_app_id(&mut dst, context, app_id.as_bytes());
    /* Set the instance id to indicate the root process */
    let pid = std::process::id();
    let mut pid_str = [0u8; 10];
    let mut pid_cursor = Cursor::new(&mut pid_str[..]);
    write!(pid_cursor, "{}", pid).unwrap();
    let pid_len = pid_cursor.position() as usize;
    write_req_wp_security_context_v1_set_instance_id(&mut dst, context, &pid_str[..pid_len]);
    write_req_wp_security_context_v1_set_sandbox_engine(&mut dst, context, "waypipe".as_bytes());
    write_req_wp_security_context_v1_commit(&mut dst, context);
    write_req_wl_display_sync(&mut dst, display, callback2);

    let msg_len = tmp_len - dst.len();
    let iovs = [IoSlice::new(&tmp[..msg_len])];
    let fds = [listen_fd.as_raw_fd(), close_fd.as_raw_fd()];
    let cmsgs = [nix::sys::socket::ControlMessage::ScmRights(&fds)];

    debug!(
        "Setting security context with app_id: {}, instance id {}",
        app_id,
        std::str::from_utf8(&pid_str[..pid_len]).unwrap()
    );
    match socket::sendmsg::<()>(
        connection.as_raw_fd(),
        &iovs,
        &cmsgs,
        nix::sys::socket::MsgFlags::empty(),
        None,
    ) {
        Err(x) => {
            return Err(tag!("Failed to write to compositor: {:?}", x));
        }
        Ok(s) => {
            assert!(s == msg_len);
        }
    }
    drop(close_fd);
    drop(listen_fd);

    /* Wait for callback2 to return. Technically this should not be necessary, since
     * in the event of failure later connections will break; but even libwayland 1.23
     * has the misbehavior of dropping trailing messages when the connection closes,
     * so a roundtrip is necessary to verify arrival. */
    loop {
        let msg = read_event(&connection)?;
        let (object_id, _length, opcode) = parse_wl_header(&msg);
        let opcode = MethodId::Event(opcode);

        if object_id == display {
            if opcode == OPCODE_WL_DISPLAY_DELETE_ID {
                continue;
            } else if opcode == OPCODE_WL_DISPLAY_ERROR {
                let (objid, code, errmsg) = parse_evt_wl_display_error(&msg)?;
                return Err(tag!(
                    "Failed to set security context: Wayland error on {}, {}: {}",
                    objid,
                    code,
                    escape_non_ascii_printable(errmsg)
                ));
            } else {
                debug!("Unexpected event from object {}: {}", object_id, opcode);
            }
        } else if object_id == callback2 {
            if opcode == OPCODE_WL_CALLBACK_DONE {
                break;
            } else {
                debug!("Unexpected event from object {}: {}", object_id, opcode);
            }
        } else if object_id == registry || object_id == callback {
            // ignore
            continue;
        } else {
            debug!("Unexpected event from object {}: {}", object_id, opcode);
        }
    }

    /* Connection is safe to drop: compositor will keep listening until close_fd is hung up on */
    drop(connection);

    Ok(())
}
