//! Bindings to event port (illumos, Solaris).

use std::io;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::Instant;

use rustix::event::{port, PollFlags, Timespec};
use rustix::fd::OwnedFd;
use rustix::io::{fcntl_getfd, fcntl_setfd, FdFlags};

use crate::{Event, PollMode};

/// Interface to event ports.
#[derive(Debug)]
pub struct Poller {
    /// File descriptor for the port instance.
    port_fd: OwnedFd,
}

impl Poller {
    /// Creates a new poller.
    pub fn new() -> io::Result<Poller> {
        let port_fd = port::create()?;
        let flags = fcntl_getfd(&port_fd)?;
        fcntl_setfd(&port_fd, flags | FdFlags::CLOEXEC)?;

        #[cfg(feature = "tracing")]
        tracing::trace!(
            port_fd = ?port_fd.as_raw_fd(),
            "new",
        );

        Ok(Poller { port_fd })
    }

    /// Whether this poller supports level-triggered events.
    pub fn supports_level(&self) -> bool {
        false
    }

    /// Whether this poller supports edge-triggered events.
    pub fn supports_edge(&self) -> bool {
        false
    }

    /// Adds a file descriptor.
    ///
    /// # Safety
    ///
    /// The `fd` must be a valid file descriptor and it must last until it is deleted.
    pub unsafe fn add(&self, fd: RawFd, ev: Event, mode: PollMode) -> io::Result<()> {
        // File descriptors don't need to be added explicitly, so just modify the interest.
        self.modify(BorrowedFd::borrow_raw(fd), ev, mode)
    }

    /// Modifies an existing file descriptor.
    pub fn modify(&self, fd: BorrowedFd<'_>, ev: Event, mode: PollMode) -> io::Result<()> {
        #[cfg(feature = "tracing")]
        let span = tracing::trace_span!(
            "modify",
            port_fd = ?self.port_fd.as_raw_fd(),
            ?fd,
            ?ev,
        );
        #[cfg(feature = "tracing")]
        let _enter = span.enter();

        let mut flags = PollFlags::empty();
        if ev.readable {
            flags |= read_flags();
        }
        if ev.writable {
            flags |= write_flags();
        }

        if mode != PollMode::Oneshot {
            return Err(crate::unsupported_error(
                "this kind of event is not supported with event ports",
            ));
        }

        unsafe {
            port::associate_fd(&self.port_fd, fd, flags, ev.key as _)?;
        }

        Ok(())
    }

    /// Deletes a file descriptor.
    pub fn delete(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        #[cfg(feature = "tracing")]
        let span = tracing::trace_span!(
            "delete",
            port_fd = ?self.port_fd.as_raw_fd(),
            ?fd,
        );
        #[cfg(feature = "tracing")]
        let _enter = span.enter();

        let result = unsafe { port::dissociate_fd(&self.port_fd, fd) };
        if let Err(e) = result {
            match e {
                rustix::io::Errno::NOENT => return Ok(()),
                _ => return Err(e.into()),
            }
        }

        Ok(())
    }

    /// Waits for I/O events with an optional deadline.
    pub fn wait_deadline(&self, events: &mut Events, deadline: Option<Instant>) -> io::Result<()> {
        #[cfg(feature = "tracing")]
        let span = tracing::trace_span!(
            "wait",
            port_fd = ?self.port_fd.as_raw_fd(),
            ?deadline,
        );
        #[cfg(feature = "tracing")]
        let _enter = span.enter();

        let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));

        // Timeout for `port::getn`. In case of overflow, use no timeout.
        let timeout = match timeout {
            Some(t) => Timespec::try_from(t).ok(),
            None => None,
        };

        // Wait for I/O events.
        //
        // NoctOS: call port_getn directly instead of through rustix.
        // port_getn can fail with ETIME *and* still hand back events: when
        // the timeout fires as an event is being dequeued, the kernel returns
        // -1/ETIME with nget = 1 (seen with truss as "= 62 [1]"). rustix
        // turns any failure into Err before recording how many events were
        // written, so that event was dropped. A PORT_SOURCE_FD association is
        // one-shot: the dropped event had already consumed it, the caller
        // never learned it must re-arm, and that descriptor was never watched
        // again. With a short periodic timeout this happens within minutes.
        let len = events.list.len();
        let spare = events.list.capacity() - len;
        let mut nget: libc::c_uint = 1;
        let c_timeout = timeout.as_ref().map(|t| libc::timespec {
            tv_sec: t.tv_sec as libc::time_t,
            tv_nsec: t.tv_nsec as libc::c_long,
        });
        let res = if spare == 0 {
            nget = 0;
            0
        } else {
            unsafe {
                libc::port_getn(
                    self.port_fd.as_raw_fd(),
                    events.list.as_mut_ptr().add(len).cast::<libc::port_event>(),
                    spare.min(u32::MAX as usize) as libc::c_uint,
                    &mut nget,
                    match c_timeout.as_ref() {
                        Some(t) => t as *const libc::timespec as *mut libc::timespec,
                        None => std::ptr::null_mut(),
                    },
                )
            }
        };
        if res < 0 {
            let err = io::Error::last_os_error();
            // On ETIME nget says how many events were delivered anyway. On
            // any other error nothing is taken from the buffer.
            if err.raw_os_error() != Some(libc::ETIME) {
                return Err(err);
            }
        }
        // SAFETY: the kernel initialised `nget` entries, and nget <= spare.
        unsafe { events.list.set_len(len + (nget as usize).min(spare)) };
        #[cfg(feature = "tracing")]
        tracing::trace!(
            port_fd = ?self.port_fd,
            res = ?events.list.len(),
            "new events"
        );

        Ok(())
    }

    /// Sends a notification to wake up the current or next `wait()` call.
    pub fn notify(&self) -> io::Result<()> {
        const PORT_SOURCE_USER: i32 = 3;

        #[cfg(feature = "tracing")]
        let span = tracing::trace_span!(
            "notify",
            port_fd = ?self.port_fd.as_raw_fd(),
        );
        #[cfg(feature = "tracing")]
        let _enter = span.enter();

        // Use port_send to send a notification to the port.
        port::send(&self.port_fd, PORT_SOURCE_USER, crate::NOTIFY_KEY as _)?;

        Ok(())
    }
}

impl AsRawFd for Poller {
    fn as_raw_fd(&self) -> RawFd {
        self.port_fd.as_raw_fd()
    }
}

impl AsFd for Poller {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.port_fd.as_fd()
    }
}

/// Poll flags for all possible readability events.
fn read_flags() -> PollFlags {
    PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::PRI
}

/// Poll flags for all possible writability events.
fn write_flags() -> PollFlags {
    PollFlags::OUT | PollFlags::HUP | PollFlags::ERR
}

/// A list of reported I/O events.
pub struct Events {
    list: Vec<port::Event>,
}

unsafe impl Send for Events {}

impl Events {
    /// Creates an empty list.
    pub fn with_capacity(cap: usize) -> Events {
        Events {
            list: Vec::with_capacity(cap),
        }
    }

    /// Iterates over I/O events.
    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.list.iter().map(|ev| {
            let flags = PollFlags::from_bits_truncate(ev.events() as _);
            Event {
                key: ev.userdata() as usize,
                readable: flags.intersects(read_flags()),
                writable: flags.intersects(write_flags()),
                extra: EventExtra { flags },
            }
        })
    }

    /// Clear the list.
    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// Get the capacity of the list.
    pub fn capacity(&self) -> usize {
        self.list.capacity()
    }
}

/// Extra information associated with an event.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct EventExtra {
    /// Flags associated with this event.
    flags: PollFlags,
}

impl EventExtra {
    /// Create a new, empty version of this struct.
    #[inline]
    pub const fn empty() -> EventExtra {
        EventExtra {
            flags: PollFlags::empty(),
        }
    }

    /// Set the interrupt flag.
    #[inline]
    pub fn set_hup(&mut self, value: bool) {
        self.flags.set(PollFlags::HUP, value);
    }

    /// Set the priority flag.
    #[inline]
    pub fn set_pri(&mut self, value: bool) {
        self.flags.set(PollFlags::PRI, value);
    }

    /// Is this an interrupt event?
    #[inline]
    pub fn is_hup(&self) -> bool {
        self.flags.contains(PollFlags::HUP)
    }

    /// Is this a priority event?
    #[inline]
    pub fn is_pri(&self) -> bool {
        self.flags.contains(PollFlags::PRI)
    }

    #[inline]
    pub fn is_connect_failed(&self) -> Option<bool> {
        Some(self.flags.contains(PollFlags::ERR) && self.flags.contains(PollFlags::HUP))
    }

    #[inline]
    pub fn is_err(&self) -> Option<bool> {
        Some(self.flags.contains(PollFlags::ERR))
    }
}
