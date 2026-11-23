//! VUID input: /dev/kbd untranslated key events + /dev/mouse firm events.
//!
//! Rust port of D3 keyboard.c/pointer.c. All ioctl constants verified
//! against illumos-gate (amd64 takes the __i386_COMPAT arms):
//!   kbio.h:  KIOC='k'<<8=0x6b00; KIOCTRANS=KIOC|30=0x6b1e (x86 arm);
//!            KIOCGTRANS=KIOC|35=0x6b23; KIOCSDIRECT=KIOC|10=0x6b0a
//!   kbd.h:   TR_ASCII=1, TR_UNTRANS_EVENT=3
//!   vuid_event.h: VUIOC='v'<<8=0x7600; VUIDSFORMAT=VUIOC|11=0x760b (x86 arm);
//!            VUID_FIRM_EVENT=1; BUT(1)=32544; LOC_X_DELTA=32640,
//!            LOC_Y_DELTA=32641, LOC_X_ABSOLUTE=32642, LOC_Y_ABSOLUTE=32643;
//!            WHEEL_DEVID=0x78 -> wheel ids live at 0x7800|n
//!   vuid_wheel.h: delta = (signed char)(value & 0xff)
//!   msio.h:  MSIOC='m'<<8=0x6d00; MSIOSRESOLUTION=MSIOC|4=0x6d04;
//!            Ms_screen_resolution { height, width }  (HEIGHT FIRST)
//!
//! Keyboard semantics: conskbd forces KB_USB into every linked driver, so
//! TR_UNTRANS_EVENT firm-event ids are USB HID usages -> hid2evdev table.
//! Typematic repeats arrive as repeated down events: de-dup via down-bitmap.
//! Pointer: relative deltas (VUID Y positive = up -> negate) AND absolute
//! (usbms USB tablets; only changed axes are sent -> remember last; scale
//! set via MSIOSRESOLUTION so values arrive in screen coordinates).

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};

use crate::hid2evdev::{usb_to_evdev, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};

// kbio.h / kbd.h (x86 arms)
const KIOCTRANS: i32 = 0x6b1e;
const KIOCGTRANS: i32 = 0x6b23;
const KIOCSDIRECT: i32 = 0x6b0a;
const TR_ASCII: libc::c_int = 1;
const TR_UNTRANS_EVENT: libc::c_int = 3;

// vuid_event.h (x86 arms)
const VUIDSFORMAT: i32 = 0x760b;
const VUID_FIRM_EVENT: libc::c_int = 1;
const BUT1: u16 = 32544;
const BUT2: u16 = 32545;
const BUT3: u16 = 32546;
const LOC_X_DELTA: u16 = 32640;
const LOC_Y_DELTA: u16 = 32641;
const LOC_X_ABSOLUTE: u16 = 32642;
const LOC_Y_ABSOLUTE: u16 = 32643;
const WHEEL_ID_ADDR: u16 = 0x7800;

// vuid_wheel.h (illumos-gate usr/src/uts/common/sys/vuid_wheel.h):
//   VUIDGWHEELCOUNT = VUIOC|15, VUIDSWHEELSTATE = VUIOC|18, with VUIOC = 'v'<<8.
const VUIDGWHEELCOUNT: i32 = 0x760f;
const VUIDSWHEELSTATE: i32 = 0x7612;
const VUID_WHEEL_STATE_VERS: libc::c_int = 1;
const VUID_WHEEL_STATE_ENABLED: u32 = 1 << 0;

/// vuid_wheel.h wheel_state.
#[repr(C)]
struct WheelState {
    vers: libc::c_int,
    id: libc::c_int,
    stateflags: u32,
}

// msio.h
const MSIOSRESOLUTION: i32 = 0x6d04;

/// sys/vuid_event.h Firm_event, LP64: 16 bytes (timeval32).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FirmEvent {
    pub id: u16,
    pub pair_type: u8,
    pub pair: u8,
    pub value: i32,
    pub time_sec: i32,
    pub time_usec: i32,
}
const _: () = assert!(std::mem::size_of::<FirmEvent>() == 16);

/// msio.h Ms_screen_resolution — height before width, per the header.
#[repr(C)]
struct MsScreenResolution {
    height: libc::c_int,
    width: libc::c_int,
}

/// A decoded input event, already in evdev vocabulary where applicable.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    /// evdev keycode, pressed?
    Key { code: u16, pressed: bool },
    /// HID usage with no evdev mapping (diagnostics)
    UnknownKey { usage: u16, pressed: bool },
    /// evdev button code (BTN_LEFT/RIGHT/MIDDLE), pressed?
    Button { code: u16, pressed: bool },
    /// relative motion, +x right, +y DOWN (VUID's +y-up already negated)
    Motion { dx: i32, dy: i32 },
    /// absolute position in screen coordinates (MSIOSRESOLUTION scale)
    MotionAbsolute { x: i32, y: i32 },
    /// vertical wheel delta (positive = away from user), wheel number
    Wheel { wheel: u8, delta: i8 },
}

fn read_events(file: &mut File, buf: &mut Vec<FirmEvent>) -> io::Result<usize> {
    // Drain the device completely. A caller that polls once per frame must
    // never leave events behind: with slow frames the kernel queue would grow
    // and input would lag further and further behind the hand.
    let mut raw = [0u8; 16 * 64];
    let mut total = 0;
    loop {
        let n = match file.read(&mut raw) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let count = n / std::mem::size_of::<FirmEvent>();
        for i in 0..count {
            let ev = unsafe {
                std::ptr::read_unaligned(raw.as_ptr().add(i * 16) as *const FirmEvent)
            };
            buf.push(ev);
        }
        total += count;
        if count == 0 {
            break;
        }
    }
    Ok(total)
}

fn open_input(path: &str) -> io::Result<File> {
    // Match the D3 C backend exactly: read-only, non-blocking, cloexec.
    // (/dev/kbd opened read-write answered an ioctl with EFAULT.)
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

fn ioctl_err(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// /dev/kbd in TR_UNTRANS_EVENT + KIOCSDIRECT mode. Restores TR_ASCII and
/// releases direct mode on Drop.
pub struct Keyboard {
    file: File,
    saved_trans: libc::c_int,
    /// 256-bit down map over HID usages, for typematic de-dup.
    down: [u64; 4],
    pending: Vec<FirmEvent>,
}

impl Keyboard {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = open_input(path)?;
        let fd = file.as_raw_fd();

        let mut saved_trans: libc::c_int = TR_ASCII;
        unsafe { libc::ioctl(fd, KIOCGTRANS as _, &mut saved_trans) };

        // Order matches D3: translation mode first (fatal), then direct
        // delivery (non-fatal there, non-fatal here).
        // All ioctl int args MUST be stack locals, not `&CONST`: a reference
        // to a const promotes to a .rodata static in release builds, and the
        // kernel's copyin on the conskbd STREAMS path faults on it (EFAULT).
        // The D3 C backend always used stack ints; match it exactly.
        let trans: libc::c_int = TR_UNTRANS_EVENT;
        if unsafe { libc::ioctl(fd, KIOCTRANS as _, &trans) } < 0 {
            return Err(ioctl_err("KIOCTRANS(TR_UNTRANS_EVENT)"));
        }
        let direct: libc::c_int = 1;
        if unsafe { libc::ioctl(fd, KIOCSDIRECT as _, &direct) } < 0 {
            eprintln!("input: KIOCSDIRECT failed (continuing): {}", io::Error::last_os_error());
        }
        Ok(Self {
            file,
            saved_trans,
            down: [0; 4],
            pending: Vec::new(),
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn down_test_set(&mut self, usage: u16, pressed: bool) -> bool {
        let (w, b) = ((usage >> 6) as usize & 3, usage & 63);
        let was = self.down[w] >> b & 1 == 1;
        if pressed {
            self.down[w] |= 1 << b;
        } else {
            self.down[w] &= !(1 << b);
        }
        was != pressed
    }

    /// Drain readable events (call on POLLIN or in a poll loop).
    pub fn poll(&mut self, out: &mut Vec<Event>) -> io::Result<()> {
        self.pending.clear();
        read_events(&mut self.file, &mut self.pending)?;
        let evs = std::mem::take(&mut self.pending);
        for ev in &evs {
            let usage = ev.id;
            let pressed = ev.value != 0;
            // typematic: repeated downs for a held key — emit only edges
            if !self.down_test_set(usage, pressed) {
                continue;
            }
            match usb_to_evdev(usage) {
                Some(code) => out.push(Event::Key { code, pressed }),
                None => out.push(Event::UnknownKey { usage, pressed }),
            }
        }
        self.pending = evs;
        Ok(())
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        let fd = self.file.as_raw_fd();
        unsafe {
            libc::ioctl(fd, KIOCTRANS as _, &self.saved_trans);
            let direct: libc::c_int = 0;
            libc::ioctl(fd, KIOCSDIRECT as _, &direct);
        }
    }
}

/// /dev/mouse in VUID_FIRM_EVENT format; handles relative and absolute
/// devices (USB tablets). Pass the screen size so MSIOSRESOLUTION scales
/// absolute coordinates to pixels.
pub struct Pointer {
    file: File,
    abs_x: i32,
    abs_y: i32,
    pending: Vec<FirmEvent>,
}

impl Pointer {
    pub fn open(path: &str, screen_w: u32, screen_h: u32) -> io::Result<Self> {
        let file = open_input(path)?;
        let fd = file.as_raw_fd();

        let format: libc::c_int = VUID_FIRM_EVENT; // stack local — see Keyboard::open
        if unsafe { libc::ioctl(fd, VUIDSFORMAT as _, &format) } < 0 {
            return Err(ioctl_err("VUIDSFORMAT(VUID_FIRM_EVENT)"));
        }
        // Scale absolute devices to the screen; harmless ENOTTY on relative.
        let res = MsScreenResolution {
            height: screen_h as libc::c_int,
            width: screen_w as libc::c_int,
        };
        unsafe { libc::ioctl(fd, MSIOSRESOLUTION as _, &res) };

        // Wheels report nothing until they are switched on, one by one. A
        // device without wheels, or one that does not know these ioctls,
        // answers with an error; there is then nothing to switch on.
        let mut wheels: libc::c_int = 0;
        if unsafe { libc::ioctl(fd, VUIDGWHEELCOUNT as _, &mut wheels) } == 0 {
            for id in 0..wheels.clamp(0, 256) {
                let state = WheelState {
                    vers: VUID_WHEEL_STATE_VERS,
                    id,
                    stateflags: VUID_WHEEL_STATE_ENABLED,
                };
                unsafe { libc::ioctl(fd, VUIDSWHEELSTATE as _, &state) };
            }
        }

        Ok(Self {
            file,
            abs_x: (screen_w / 2) as i32,
            abs_y: (screen_h / 2) as i32,
            pending: Vec::new(),
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Drain readable events. Coalesces per read batch: deltas accumulate,
    /// absolute axes remember the other coordinate (only changed axes are
    /// sent by usbms).
    pub fn poll(&mut self, out: &mut Vec<Event>) -> io::Result<()> {
        self.pending.clear();
        read_events(&mut self.file, &mut self.pending)?;
        let evs = std::mem::take(&mut self.pending);
        let (mut dx, mut dy) = (0i32, 0i32);
        let mut abs_dirty = false;
        for ev in &evs {
            match ev.id {
                BUT1 => out.push(Event::Button { code: BTN_LEFT, pressed: ev.value != 0 }),
                BUT2 => out.push(Event::Button { code: BTN_MIDDLE, pressed: ev.value != 0 }),
                BUT3 => out.push(Event::Button { code: BTN_RIGHT, pressed: ev.value != 0 }),
                LOC_X_DELTA => dx += ev.value,
                // VUID: positive = up; evdev/screen: positive = down.
                LOC_Y_DELTA => dy -= ev.value,
                LOC_X_ABSOLUTE => {
                    self.abs_x = ev.value;
                    abs_dirty = true;
                }
                LOC_Y_ABSOLUTE => {
                    self.abs_y = ev.value;
                    abs_dirty = true;
                }
                id if id & 0xff00 == WHEEL_ID_ADDR => out.push(Event::Wheel {
                    wheel: (id & 0xff) as u8,
                    delta: (ev.value & 0xff) as u8 as i8,
                }),
                _ => {}
            }
        }
        if dx != 0 || dy != 0 {
            out.push(Event::Motion { dx, dy });
        }
        if abs_dirty {
            out.push(Event::MotionAbsolute { x: self.abs_x, y: self.abs_y });
        }
        self.pending = evs;
        Ok(())
    }
}
