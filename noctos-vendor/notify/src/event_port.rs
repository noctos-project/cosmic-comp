//! Watcher implementation for illumos and Solaris event ports
//!
//! Event ports can watch files and directories directly (`PORT_SOURCE_FILE`,
//! also known as File Events Notification). The interface, as documented in
//! `port_associate(3C)` and at the top of illumos'
//! `usr/src/uts/common/fs/portfs/port_fop.c`:
//!
//! * A watch is a `file_obj` naming a path, together with the time stamps a
//!   `stat(2)` of it returned. If the file's stamps differ from those when the
//!   watch is registered, the event is delivered at once, so nothing is lost
//!   between looking at a file and watching it.
//! * The kernel identifies the watch by the address of the `file_obj`, which
//!   therefore has to stay where it is while the watch exists.
//! * A watch is one-shot: after an event it must be registered again.
//! * Deleting or renaming the file (or renaming another file over it) delivers
//!   an exception event and ends the watch.
//! * A directory counts as modified when an entry is added to it or removed
//!   from it. Nothing says which, so the directory is read again and compared
//!   with what was there before.
//!
//! There is one watch per file. To report changes to the files in a watched
//! directory, as the other backends do, every entry gets a watch of its own.

use super::event::*;
use super::{Config, Error, EventHandler, RecursiveMode, Result, Watcher};
use crate::{unbounded, Receiver, Sender};
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

/// Declarations from `<port.h>` / `<sys/port.h>`.
///
/// The values and layouts are those of illumos-gate's
/// `usr/src/uts/common/sys/port.h`. They are declared here because the `libc`
/// crate has the port functions but neither `file_obj` nor the file events.
#[allow(non_camel_case_types)]
mod sys {
    use libc::{c_char, c_int, c_uint, c_ushort, c_void, timespec, uintptr_t};

    pub const PORT_SOURCE_USER: c_ushort = 3;
    pub const PORT_SOURCE_FILE: c_ushort = 7;

    // Events a watch can ask for.
    pub const FILE_MODIFIED: c_int = 0x0000_0002;
    pub const FILE_ATTRIB: c_int = 0x0000_0004;
    /// Watch a symbolic link itself, not what it points to.
    pub const FILE_NOFOLLOW: c_int = 0x1000_0000;

    // Exception events. They are always delivered and end the watch.
    pub const FILE_DELETE: c_int = 0x0000_0010;
    pub const FILE_RENAME_TO: c_int = 0x0000_0020;
    pub const FILE_RENAME_FROM: c_int = 0x0000_0040;
    pub const UNMOUNTED: c_int = 0x2000_0000;
    pub const MOUNTEDOVER: c_int = 0x4000_0000;
    pub const FILE_EXCEPTION: c_int =
        UNMOUNTED | FILE_DELETE | FILE_RENAME_TO | FILE_RENAME_FROM | MOUNTEDOVER;

    #[repr(C)]
    pub struct file_obj {
        pub fo_atime: timespec,
        pub fo_mtime: timespec,
        pub fo_ctime: timespec,
        pub fo_pad: [uintptr_t; 3],
        pub fo_name: *mut c_char,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct port_event {
        pub portev_events: c_int,
        pub portev_source: c_ushort,
        pub portev_pad: c_ushort,
        pub portev_object: uintptr_t,
        pub portev_user: *mut c_void,
    }

    extern "C" {
        pub fn port_create() -> c_int;
        pub fn port_associate(
            port: c_int,
            source: c_int,
            object: uintptr_t,
            events: c_int,
            user: *mut c_void,
        ) -> c_int;
        pub fn port_dissociate(port: c_int, source: c_int, object: uintptr_t) -> c_int;
        pub fn port_getn(
            port: c_int,
            list: *mut port_event,
            max: c_uint,
            nget: *mut c_uint,
            timeout: *mut timespec,
        ) -> c_int;
        pub fn port_send(port: c_int, events: c_int, user: *mut c_void) -> c_int;
    }
}

/// One watched file or directory.
struct Watch {
    /// What the kernel was given. Lives in a `Box` so that its address, which
    /// is the kernel's name for the watch, never changes.
    fobj: sys::file_obj,
    /// Owns the memory `fobj.fo_name` points to.
    _name: CString,
    path: PathBuf,
    is_dir: bool,
    /// For a directory: whether its entries are watched too.
    watch_entries: bool,
    /// For a directory with `watch_entries`: whether that extends to the
    /// directories below it.
    recursive: bool,
    /// For a directory with `watch_entries`: the names found in it last time.
    entries: HashSet<OsString>,
}

// SAFETY: the raw pointer in `fobj` points into `_name`, which the same value
// owns and never changes, so a `Watch` can move to the event loop's thread.
unsafe impl Send for Watch {}

// The EventLoop owns the port and waits on it for
//
// -  file events on the watched files, and
//
// -  user events, sent with port_send() to say that a message is waiting.
struct EventLoop {
    running: bool,
    port: Arc<OwnedFd>,
    event_loop_tx: Sender<EventLoopMsg>,
    event_loop_rx: Receiver<EventLoopMsg>,
    event_handler: Box<dyn EventHandler>,
    /// Every watch, by the id handed to the kernel as the user cookie.
    watches: HashMap<usize, Box<Watch>>,
    ids: HashMap<PathBuf, usize>,
    next_id: usize,
    follow_symlinks: bool,
}

/// Watcher implementation based on event ports
#[derive(Debug)]
pub struct EventPortWatcher {
    channel: Sender<EventLoopMsg>,
    port: Arc<OwnedFd>,
}

enum EventLoopMsg {
    AddWatch(PathBuf, RecursiveMode, Sender<Result<()>>),
    RemoveWatch(PathBuf, Sender<Result<()>>),
    Shutdown,
}

/// Tell the event loop that a message is waiting for it.
fn wake(port: RawFd) -> io::Result<()> {
    if unsafe { sys::port_send(port, 0, std::ptr::null_mut()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn timespec(sec: i64, nsec: i64) -> libc::timespec {
    // SAFETY: timespec is plain data; zero is a valid value for any padding.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    ts.tv_sec = sec as _;
    ts.tv_nsec = nsec as _;
    ts
}

impl EventLoop {
    fn new(event_handler: Box<dyn EventHandler>, follow_symlinks: bool) -> Result<Self> {
        let port = unsafe { sys::port_create() };
        if port < 0 {
            return Err(Error::io(io::Error::last_os_error()));
        }
        // SAFETY: port_create returned a new descriptor that nothing else owns.
        let port = unsafe { OwnedFd::from_raw_fd(port) };
        unsafe { libc::fcntl(port.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };

        let (event_loop_tx, event_loop_rx) = unbounded::<EventLoopMsg>();
        Ok(EventLoop {
            running: true,
            port: Arc::new(port),
            event_loop_tx,
            event_loop_rx,
            event_handler,
            watches: HashMap::new(),
            ids: HashMap::new(),
            next_id: 1,
            follow_symlinks,
        })
    }

    // Run the event loop.
    fn run(self) {
        let _ = thread::Builder::new()
            .name("notify-rs event port loop".to_string())
            .spawn(|| self.event_loop_thread());
    }

    fn event_loop_thread(mut self) {
        // SAFETY: port_event is plain data.
        let mut events: [sys::port_event; 64] = unsafe { std::mem::zeroed() };
        while self.running {
            // Wait for at least one event, take as many as are there.
            let mut nget: libc::c_uint = 1;
            let ret = unsafe {
                sys::port_getn(
                    self.port.as_raw_fd(),
                    events.as_mut_ptr(),
                    events.len() as libc::c_uint,
                    &mut nget,
                    std::ptr::null_mut(),
                )
            };
            if ret < 0 {
                match io::Error::last_os_error().raw_os_error() {
                    // Interrupted, or timed out with some events retrieved.
                    Some(libc::EINTR) | Some(libc::ETIME) => {}
                    // The port is unusable; there is nothing left to wait for.
                    _ => return,
                }
            }

            let mut messages = false;
            for event in &events[..(nget as usize).min(events.len())] {
                match event.portev_source {
                    sys::PORT_SOURCE_USER => messages = true,
                    sys::PORT_SOURCE_FILE => {
                        self.handle_file_event(event.portev_user as usize, event.portev_events)
                    }
                    _ => {}
                }
            }
            if messages {
                self.handle_messages();
            }
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.event_loop_rx.try_recv() {
            match msg {
                EventLoopMsg::AddWatch(path, recursive_mode, tx) => {
                    let _ = tx.send(self.add_watch(path, recursive_mode.is_recursive()));
                }
                EventLoopMsg::RemoveWatch(path, tx) => {
                    let _ = tx.send(self.remove_watch(path));
                }
                EventLoopMsg::Shutdown => {
                    self.running = false;
                    break;
                }
            }
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        if self.follow_symlinks {
            fs::metadata(path)
        } else {
            fs::symlink_metadata(path)
        }
    }

    fn emit(&mut self, kind: EventKind, path: PathBuf) {
        self.event_handler
            .handle_event(Ok(Event::new(kind).add_path(path)));
    }

    /// Register, or register again, the watch with this id, using the time
    /// stamps the file has now.
    ///
    /// Callers report an event only after this, never before: a change made
    /// before the `stat` here is covered by the event about to be reported, and
    /// one made after it is caught by the kernel, which compares these stamps
    /// with the file's when it registers the watch.
    fn associate(&mut self, id: usize) -> io::Result<()> {
        let follow_symlinks = self.follow_symlinks;
        let port = self.port.as_raw_fd();
        let Some(watch) = self.watches.get_mut(&id) else {
            return Ok(());
        };
        let meta = if follow_symlinks {
            fs::metadata(&watch.path)?
        } else {
            fs::symlink_metadata(&watch.path)?
        };
        watch.fobj.fo_atime = timespec(meta.atime(), meta.atime_nsec());
        watch.fobj.fo_mtime = timespec(meta.mtime(), meta.mtime_nsec());
        watch.fobj.fo_ctime = timespec(meta.ctime(), meta.ctime_nsec());

        let mut events = sys::FILE_MODIFIED | sys::FILE_ATTRIB;
        if !follow_symlinks {
            events |= sys::FILE_NOFOLLOW;
        }
        let object = &watch.fobj as *const sys::file_obj as libc::uintptr_t;
        let ret = unsafe {
            sys::port_associate(
                port,
                sys::PORT_SOURCE_FILE as libc::c_int,
                object,
                events,
                id as *mut libc::c_void,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Watch one path. Returns whether it is a directory.
    ///
    /// A path that is watched already keeps its watch; the flags are only ever
    /// widened, so that overlapping watches do not take anything away from
    /// each other.
    fn add_single_watch(
        &mut self,
        path: &Path,
        watch_entries: bool,
        recursive: bool,
    ) -> io::Result<bool> {
        if let Some(watch) = self.ids.get(path).and_then(|id| self.watches.get_mut(id)) {
            watch.watch_entries |= watch_entries && watch.is_dir;
            watch.recursive |= recursive && watch.is_dir;
            return Ok(watch.is_dir);
        }

        let is_dir = self.metadata(path)?.is_dir();
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        log::trace!("adding event port watch: {}", path.display());

        let id = self.next_id;
        self.next_id += 1;
        let watch = Box::new(Watch {
            fobj: sys::file_obj {
                fo_atime: timespec(0, 0),
                fo_mtime: timespec(0, 0),
                fo_ctime: timespec(0, 0),
                fo_pad: [0; 3],
                fo_name: name.as_ptr() as *mut libc::c_char,
            },
            _name: name,
            path: path.to_owned(),
            is_dir,
            watch_entries: watch_entries && is_dir,
            recursive: recursive && is_dir,
            entries: HashSet::new(),
        });
        self.watches.insert(id, watch);
        self.ids.insert(path.to_owned(), id);
        if let Err(err) = self.associate(id) {
            self.forget(id);
            return Err(err);
        }
        Ok(is_dir)
    }

    /// Bring the watches on the entries of a directory up to date with what
    /// the directory contains now.
    ///
    /// With `report`, entries that were not there at the last look are reported
    /// as created, and so is everything inside a new directory when the watch
    /// is recursive: those files appeared before they could be watched.
    ///
    /// Entries that are gone need nothing done here. Each had a watch of its
    /// own, which has delivered or will deliver the delete or rename.
    fn scan_directory(&mut self, id: usize, report: bool) {
        let Some(watch) = self.watches.get(&id) else {
            return;
        };
        if !watch.is_dir || !watch.watch_entries {
            return;
        }
        let dir = watch.path.clone();
        let recursive = watch.recursive;

        let Ok(read_dir) = fs::read_dir(&dir) else {
            return;
        };
        let names: HashSet<OsString> = read_dir
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name())
            .collect();

        let new: Vec<OsString> = {
            let known = &self.watches[&id].entries;
            names.iter().filter(|n| !known.contains(*n)).cloned().collect()
        };
        if let Some(watch) = self.watches.get_mut(&id) {
            watch.entries = names;
        }

        for name in new {
            let path = dir.join(&name);
            // Gone again already, or not ours to look at: nothing to watch.
            let Ok(is_dir) = self.add_single_watch(&path, recursive, recursive) else {
                continue;
            };
            if report {
                let kind = if is_dir {
                    CreateKind::Folder
                } else {
                    CreateKind::File
                };
                self.emit(EventKind::Create(kind), path.clone());
            }
            if is_dir && recursive {
                if let Some(child) = self.ids.get(&path).copied() {
                    self.scan_directory(child, report);
                }
            }
        }
    }

    fn add_watch(&mut self, path: PathBuf, is_recursive: bool) -> Result<()> {
        // A watch on a directory covers its entries; a recursive one covers
        // everything below as well.
        self.add_single_watch(&path, true, is_recursive)
            .map_err(|e| Error::io(e).add_path(path.clone()))?;
        if let Some(id) = self.ids.get(&path).copied() {
            self.scan_directory(id, false);
        }
        Ok(())
    }

    /// Take a path out of, or put it back into, what its parent directory is
    /// known to contain. A path taken out is looked at afresh by the next scan
    /// of the directory, which matters when a file is deleted and created
    /// again before that scan happens.
    fn set_known_to_parent(&mut self, path: &Path, known: bool) {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let Some(id) = self.ids.get(parent).copied() else {
            return;
        };
        if let Some(watch) = self.watches.get_mut(&id) {
            if !watch.watch_entries {
                return;
            }
            if known {
                watch.entries.insert(name.to_owned());
            } else {
                watch.entries.remove(name);
            }
        }
    }

    /// Drop a watch from the bookkeeping and from the port.
    fn forget(&mut self, id: usize) {
        if let Some(watch) = self.watches.remove(&id) {
            log::trace!("removing event port watch: {}", watch.path.display());
            self.ids.remove(&watch.path);
            // Fails with ENOENT when the watch has fired and not been
            // registered again, or an exception event ended it. Either way it
            // is gone, which is all that is wanted here.
            unsafe {
                sys::port_dissociate(
                    self.port.as_raw_fd(),
                    sys::PORT_SOURCE_FILE as libc::c_int,
                    &watch.fobj as *const sys::file_obj as libc::uintptr_t,
                );
            }
        }
    }

    fn remove_watch(&mut self, path: PathBuf) -> Result<()> {
        let Some(id) = self.ids.get(&path).copied() else {
            return Err(Error::watch_not_found());
        };
        let covers_entries = self.watches.get(&id).is_some_and(|w| w.watch_entries);
        self.forget(id);
        if covers_entries {
            let below: Vec<usize> = self
                .ids
                .iter()
                .filter(|(p, _)| p.starts_with(&path))
                .map(|(_, id)| *id)
                .collect();
            for id in below {
                self.forget(id);
            }
        }
        Ok(())
    }

    fn handle_file_event(&mut self, id: usize, events: libc::c_int) {
        // An event for a watch dropped since it was queued.
        let Some(watch) = self.watches.get(&id) else {
            return;
        };
        let path = watch.path.clone();
        let is_dir = watch.is_dir;
        let watch_entries = watch.watch_entries;
        let recursive = watch.recursive;
        log::trace!("event port event: {events:#x} {}", path.display());

        if events & sys::FILE_EXCEPTION != 0 {
            // The file this watch was on is no longer what the path names, and
            // the kernel has ended the watch.
            self.forget(id);
            self.set_known_to_parent(&path, false);

            if events & (sys::FILE_DELETE | sys::UNMOUNTED) != 0 {
                let kind = if is_dir {
                    RemoveKind::Folder
                } else {
                    RemoveKind::File
                };
                self.emit(EventKind::Remove(kind), path.clone());
            }
            if events & sys::FILE_RENAME_FROM != 0 {
                self.emit(
                    EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                    path.clone(),
                );
            }

            // Another file may have taken the path: one renamed over it (how
            // files are replaced atomically), one created in its place, or a
            // file system mounted on it. Watch that one, then say so.
            if let Ok(now_dir) = self.add_single_watch(&path, watch_entries, recursive) {
                self.set_known_to_parent(&path, true);
                if events & (sys::FILE_RENAME_TO | sys::MOUNTEDOVER) != 0 {
                    self.emit(
                        EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                        path.clone(),
                    );
                } else {
                    // Deleted or moved away, and something new is there already.
                    let kind = if now_dir {
                        CreateKind::Folder
                    } else {
                        CreateKind::File
                    };
                    self.emit(EventKind::Create(kind), path.clone());
                }
                if let Some(id) = self.ids.get(&path).copied() {
                    self.scan_directory(id, true);
                }
            }
            return;
        }

        // Register again before reporting; see `associate`.
        if self.associate(id).is_err() {
            // It went away in the meantime. If it was deleted while unwatched
            // nothing else will report it.
            self.forget(id);
            self.set_known_to_parent(&path, false);
            let kind = if is_dir {
                RemoveKind::Folder
            } else {
                RemoveKind::File
            };
            self.emit(EventKind::Remove(kind), path);
            return;
        }

        if events & sys::FILE_MODIFIED != 0 {
            if is_dir {
                // Entries were added or removed. Removals are reported by the
                // watches on the entries themselves.
                self.scan_directory(id, true);
            } else {
                self.emit(EventKind::Modify(ModifyKind::Data(DataChange::Any)), path);
            }
        } else if events & sys::FILE_ATTRIB != 0 {
            self.emit(
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
                path,
            );
        }
    }
}

impl EventPortWatcher {
    fn from_event_handler(
        event_handler: Box<dyn EventHandler>,
        follow_symlinks: bool,
    ) -> Result<Self> {
        let event_loop = EventLoop::new(event_handler, follow_symlinks)?;
        let channel = event_loop.event_loop_tx.clone();
        let port = event_loop.port.clone();
        event_loop.run();
        Ok(EventPortWatcher { channel, port })
    }

    fn absolute(path: &Path) -> Result<PathBuf> {
        if path.is_absolute() {
            Ok(path.to_owned())
        } else {
            let p = env::current_dir().map_err(Error::io)?;
            Ok(p.join(path))
        }
    }

    fn request(&self, msg: EventLoopMsg, rx: Receiver<Result<()>>) -> Result<()> {
        self.channel
            .send(msg)
            .map_err(|e| Error::generic(&e.to_string()))?;
        wake(self.port.as_raw_fd()).map_err(Error::io)?;
        rx.recv().map_err(|e| Error::generic(&e.to_string()))?
    }

    fn watch_inner(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::AddWatch(Self::absolute(path)?, recursive_mode, tx);
        self.request(msg, rx)
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::RemoveWatch(Self::absolute(path)?, tx);
        self.request(msg, rx)
    }
}

impl Watcher for EventPortWatcher {
    /// Create a new watcher.
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        Self::from_event_handler(Box::new(event_handler), config.follow_symlinks())
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::EventPort
    }
}

impl Drop for EventPortWatcher {
    fn drop(&mut self) {
        // The event loop may be gone already if its port became unusable.
        let _ = self.channel.send(EventLoopMsg::Shutdown);
        let _ = wake(self.port.as_raw_fd());
    }
}
