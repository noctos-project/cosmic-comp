//! illumos: readiness of client sockets, delivered through a pipe.
//!
//! The backend has to hand its caller ONE descriptor that becomes readable
//! when any client has something to say. On Linux that is an epoll
//! descriptor and on the BSDs a kqueue, both of which can themselves be
//! polled. On illumos neither obvious choice is safe to nest inside the
//! caller's own poller:
//!
//! * an epoll descriptor never wakes anything but another epoll//dev/poll;
//! * an event port can be polled, but whether a poller is woken depends on
//!   how the event got queued (port_associate of an already-ready descriptor
//!   does not), and port_getn itself wakes pollers. Two workarounds for that
//!   still left clients unserviced in the field.
//!
//! So nothing polls the event port here. A helper thread is its only reader:
//! it blocks in port_getn(), which is the one fully ordinary way to consume a
//! port, records which clients are ready and writes a byte to a pipe. The
//! pipe's read end is the descriptor given to the caller; pipes are polled by
//! every event loop already.

use std::{
    io,
    os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{Arc, Mutex},
};

pub(crate) struct ClientPort {
    port: OwnedFd,
    wake_w: OwnedFd,
    /// `portev_user` values (client ids) reported ready and not yet taken.
    ready: Mutex<Vec<usize>>,
}

/// Live ports, keyed by the raw descriptor of the pipe's read end (which is
/// what the backend stores as its `poll_fd`).
static PORTS: Mutex<Vec<(RawFd, Arc<ClientPort>)>> = Mutex::new(Vec::new());

fn cloexec_nonblock(fd: RawFd) {
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

/// Create the port, the pipe and the helper thread. Returns the pipe's read
/// end: the descriptor the caller's event loop should watch.
pub(crate) fn create() -> io::Result<OwnedFd> {
    let port = unsafe { libc::port_create() };
    if port < 0 {
        return Err(io::Error::last_os_error());
    }
    let port = unsafe { OwnedFd::from_raw_fd(port) };
    unsafe { libc::fcntl(port.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let wake_r = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let wake_w = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    cloexec_nonblock(wake_r.as_raw_fd());
    cloexec_nonblock(wake_w.as_raw_fd());

    let shared = Arc::new(ClientPort { port, wake_w, ready: Mutex::new(Vec::new()) });
    {
        let mut ports = PORTS.lock().unwrap();
        // A previous backend that used this descriptor number is gone.
        ports.retain(|(fd, _)| *fd != wake_r.as_raw_fd());
        ports.push((wake_r.as_raw_fd(), shared.clone()));
    }

    let thread_shared = shared;
    std::thread::Builder::new()
        .name("wl-client-port".into())
        .spawn(move || reader(thread_shared))?;

    Ok(wake_r)
}

fn reader(shared: Arc<ClientPort>) {
    let mut events: [libc::port_event; 32] = unsafe { std::mem::zeroed() };
    loop {
        // Block until at least one event is available.
        let mut nget: libc::c_uint = 1;
        let ret = unsafe {
            libc::port_getn(
                shared.port.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as libc::c_uint,
                &mut nget,
                std::ptr::null_mut(),
            )
        };
        if ret < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) | Some(libc::ETIME) => {}
                // The port is gone or unusable: nothing more to do.
                _ => return,
            }
        }
        if nget == 0 {
            continue;
        }
        {
            let mut ready = shared.ready.lock().unwrap();
            for event in &events[..nget as usize] {
                if event.portev_source as libc::c_int == libc::PORT_SOURCE_FD {
                    ready.push(event.portev_user as usize);
                }
            }
        }
        // Readiness is recorded before the byte is written, so a caller that
        // sees the pipe readable always finds the ids. A full pipe (EAGAIN)
        // already guarantees a wake-up; a closed read end (EPIPE) means the
        // backend was dropped.
        let byte = 1u8;
        let n = unsafe {
            libc::write(shared.wake_w.as_raw_fd(), &byte as *const u8 as *const libc::c_void, 1)
        };
        if n < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EPIPE) {
            let mut ports = PORTS.lock().unwrap();
            ports.retain(|(_, p)| !Arc::ptr_eq(p, &shared));
            return;
        }
    }
}

pub(crate) fn lookup(poll_fd: RawFd) -> Option<Arc<ClientPort>> {
    PORTS.lock().unwrap().iter().find(|(fd, _)| *fd == poll_fd).map(|(_, p)| p.clone())
}

impl ClientPort {
    /// Watch `fd` for input once. The association is consumed by the event it
    /// delivers; call again after handling it. If input is already pending
    /// the event is queued immediately and the helper thread picks it up.
    pub(crate) fn watch(&self, fd: RawFd, user: usize) -> io::Result<()> {
        let r = unsafe {
            libc::port_associate(
                self.port.as_raw_fd(),
                libc::PORT_SOURCE_FD,
                fd as libc::uintptr_t,
                libc::POLLIN as libc::c_int,
                user as *mut libc::c_void,
            )
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Stop watching `fd`. Usually a no-op (ENOENT): the association was
    /// consumed by the event that led here, or removed when `fd` was closed.
    pub(crate) fn unwatch(&self, fd: RawFd) {
        unsafe {
            libc::port_dissociate(
                self.port.as_raw_fd(),
                libc::PORT_SOURCE_FD,
                fd as libc::uintptr_t,
            );
        }
    }

    /// Drain the wake-up pipe, then take the ids reported ready. Order
    /// matters: an id recorded after the take leaves its byte in the pipe,
    /// so the caller is woken again.
    pub(crate) fn take_ready(&self, poll_fd: RawFd) -> Vec<usize> {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe {
                libc::read(poll_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n < buf.len() as isize {
                break;
            }
        }
        std::mem::take(&mut *self.ready.lock().unwrap())
    }
}
