//! Network-namespace policy helpers for the runner.
//!
//! The new netns is created in [`super::userns::setup_namespaces`] via
//! `CLONE_NEWNET`; this module decides what state to leave it in
//! before the action runs.
//!
//! Phase 2 / M5 supports two policies (see [`NetworkPolicy`]):
//!
//! - `None` — empty netns, no interfaces. The action sees `lo` only as
//!   a `DOWN` link; any `connect()` returns `ENETUNREACH`.
//! - `Loopback` — bring `lo` `UP` so `127.0.0.1` and `::1` work.
//!
//! Bringing `lo` up requires `CAP_NET_ADMIN` *inside* the netns.
//! Because the new netns is owned by the new user namespace (we
//! unshared them together), the runner has `CAP_NET_ADMIN` here even
//! though it has none on the host.
//!
//! We hand-roll the netlink message rather than pull in `rtnetlink` —
//! it's two `repr(C)` structs, ~50 bytes on the wire, and the alternative
//! drags in a tokio-flavoured async stack we don't need.

use std::io;
use std::mem;

use nix::libc;

use crate::config::NetworkPolicy;

/// Apply `policy` to the runner's current network namespace. Must be
/// called after [`super::userns::setup_namespaces`] so we're inside the
/// fresh netns.
pub(super) fn apply_policy(policy: NetworkPolicy) -> io::Result<()> {
    match policy {
        NetworkPolicy::None => Ok(()),
        NetworkPolicy::Loopback => bring_loopback_up(),
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NlMsgHdr {
    len: u32,
    ty: u16,
    flags: u16,
    seq: u32,
    pid: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IfInfoMsg {
    family: u8,
    _pad: u8,
    ty: u16,
    index: i32,
    flags: u32,
    change: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinkUpRequest {
    nl: NlMsgHdr,
    ifi: IfInfoMsg,
}

const RTM_NEWLINK: u16 = 16;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;

/// Send `RTM_NEWLINK` with `IFF_UP` set on the loopback interface, then
/// read the ack and surface any kernel-reported error.
fn bring_loopback_up() -> io::Result<()> {
    // Look up the lo ifindex inside the current netns. In a freshly
    // unshared netns lo is index 1 in practice, but querying via
    // if_nametoindex (which does an SIOCGIFINDEX ioctl) is the
    // kernel-blessed way and shields us from kernels that ever change
    // that.
    //
    // SAFETY: `c"lo"` is a NUL-terminated CStr literal; the call
    // returns 0 on error and we surface that via last_os_error.
    #[allow(unsafe_code)]
    let lo_index = unsafe { libc::if_nametoindex(c"lo".as_ptr()) };
    if lo_index == 0 {
        return Err(io::Error::last_os_error());
    }
    let lo_index = lo_index as i32;

    // SAFETY: socket() returns -1 on error, otherwise a valid fd we
    // own. The fd is closed via `Fd::drop` at the end of this function
    // regardless of which path we take.
    #[allow(unsafe_code)]
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = Fd(raw);

    let req = LinkUpRequest {
        nl: NlMsgHdr {
            len: mem::size_of::<LinkUpRequest>() as u32,
            ty: RTM_NEWLINK,
            flags: NLM_F_REQUEST | NLM_F_ACK,
            seq: 1,
            pid: 0,
        },
        ifi: IfInfoMsg {
            family: libc::AF_UNSPEC as u8,
            _pad: 0,
            ty: 0,
            index: lo_index,
            flags: libc::IFF_UP as u32,
            change: libc::IFF_UP as u32,
        },
    };

    // Destination is the kernel: pid=0, groups=0.
    // SAFETY: zeroing a sockaddr_nl is valid; AF_NETLINK is a u16.
    #[allow(unsafe_code)]
    let mut sa: libc::sockaddr_nl = unsafe { mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;

    // SAFETY: req is `repr(C)` and lives on the stack for the call;
    // sa is a properly-initialised sockaddr_nl. sendto returns -1 on
    // error and the byte count on success.
    #[allow(unsafe_code)]
    let sent = unsafe {
        libc::sendto(
            fd.0,
            (&req as *const LinkUpRequest).cast::<libc::c_void>(),
            mem::size_of::<LinkUpRequest>(),
            0,
            (&sa as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }

    // Read the ack. The kernel always replies with an NLMSG_ERROR for
    // requests that set NLM_F_ACK; an errno of 0 means success.
    let mut buf = [0u8; 4096];
    // SAFETY: buf is a writable byte slice we own.
    #[allow(unsafe_code)]
    let n = unsafe { libc::recv(fd.0, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;
    if n < mem::size_of::<NlMsgHdr>() + mem::size_of::<i32>() {
        return Err(io::Error::other(format!("short netlink ack: {n} bytes")));
    }
    let reply_ty = u16::from_ne_bytes([buf[4], buf[5]]);
    if reply_ty != NLMSG_ERROR {
        return Err(io::Error::other(format!(
            "unexpected netlink reply type: {reply_ty}"
        )));
    }
    let errno_field = i32::from_ne_bytes([
        buf[mem::size_of::<NlMsgHdr>()],
        buf[mem::size_of::<NlMsgHdr>() + 1],
        buf[mem::size_of::<NlMsgHdr>() + 2],
        buf[mem::size_of::<NlMsgHdr>() + 3],
    ]);
    if errno_field != 0 {
        // Kernel reports negative errno.
        return Err(io::Error::from_raw_os_error(-errno_field));
    }

    Ok(())
}

/// Owning wrapper around a raw fd that closes on drop. Used here to
/// keep the netlink socket lifecycle obvious even on error paths.
struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: self.0 was returned by socket() above and we are its
        // sole owner.
        #[allow(unsafe_code)]
        unsafe {
            libc::close(self.0);
        }
    }
}
