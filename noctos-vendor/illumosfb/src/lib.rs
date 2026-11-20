//! illumosfb — the illumos console framebuffer as a Rust scanout target.
//!
//! Rust port of the D3 illumosfb output layer (wlroots backend/illumosfb/output.c):
//! open /dev/fb, FBIOGATTR for geometry (fbtype) and the gfxfb_info block that
//! gfx drivers pack into fbgattr.sattr.dev_specific (pitch + RGB masks),
//! KDSETMODE(KD_GRAPHICS) handoff, mmap, row-blit present. KD_TEXT is restored
//! on Drop (including unwind). This is the permanent fb half of the future
//! smithay/cosmic-comp backend (S-ladder S1).
//!
//! The code compiles on any unix (type-check on Linux, run on illumos); the
//! ioctl constants are illumos's, verified against illumos-gate (not the D3
//! syntax shims, whose guessed KDIOC cost us an ENXIO):
//! FBIOGATTR = FIOC|6 = 0x4606 ('F'<<8), KDSETMODE = KDIOC|10 = 0x4b0a ('K'<<8).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;

pub const FBIOGATTR: i32 = 0x4606;
pub const KDSETMODE: i32 = 0x4b0a;
pub const KD_TEXT: libc::c_int = 0;
pub const KD_GRAPHICS: libc::c_int = 1;

const FB_ATTR_NDEVSPECIFIC: usize = 8;
const FB_ATTR_NEMUTYPES: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbType {
    pub fb_type: libc::c_int,
    pub fb_height: libc::c_int,
    pub fb_width: libc::c_int,
    pub fb_depth: libc::c_int,
    pub fb_cmsize: libc::c_int,
    pub fb_size: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSAttr {
    pub flags: libc::c_int,
    pub emu_type: libc::c_int,
    pub dev_specific: [libc::c_int; FB_ATTR_NDEVSPECIFIC],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbGAttr {
    pub real_type: libc::c_int,
    pub owner: libc::c_int,
    pub fbtype: FbType,
    pub sattr: FbSAttr,
    pub emu_types: [libc::c_int; FB_ATTR_NEMUTYPES],
}

/// What gfx drivers (gfxfb and friends) pack into sattr.dev_specific.
/// Layout mirrors illumos gfx_private.h gfxfb_info; naturally aligned,
/// 18 bytes of payload inside the 32-byte dev_specific area.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GfxfbInfo {
    pub terminal_origin_x: u16,
    pub terminal_origin_y: u16,
    pub pitch: u32,
    pub font_width: u16,
    pub font_height: u16,
    pub red_mask_size: u8,
    pub red_field_position: u8,
    pub green_mask_size: u8,
    pub green_field_position: u8,
    pub blue_mask_size: u8,
    pub blue_field_position: u8,
}

/// Pixel geometry of the mapped framebuffer, derived from FBIOGATTR.
#[derive(Clone, Copy, Debug)]
pub struct FbMode {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    /// Bytes per scanline (from gfxfb_info; falls back to width*bytespp).
    pub pitch: u32,
    pub red: (u8, u8),   // (mask_size, field_position)
    pub green: (u8, u8),
    pub blue: (u8, u8),
    /// Total mappable bytes (fb_size).
    pub size: usize,
}

impl FbMode {
    /// True for the X8R8G8B8 layout everything downstream assumes (DRM XR24).
    pub fn is_xrgb8888(&self) -> bool {
        self.depth == 32
            && self.red == (8, 16)
            && self.green == (8, 8)
            && self.blue == (8, 0)
    }
}

/// An open, mapped console framebuffer in graphics mode.
///
/// Dropping it unmaps and restores KD_TEXT, so a panic unwinds back to a
/// usable console. (SIGKILL skips Drop: the console stays in graphics mode
/// until something sets KD_TEXT again — rerunning any user of this crate
/// does.)
pub struct Framebuffer {
    file: File,
    mode: FbMode,
    base: *mut u8,
    graphics: bool,
}

// The mapping is owned exclusively by this struct.
unsafe impl Send for Framebuffer {}

impl Framebuffer {
    /// Open `path` (normally /dev/fb), read the mode, map the framebuffer.
    /// Does NOT switch the console mode yet — call `set_graphics()`.
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let mut gattr: FbGAttr = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::ioctl(file.as_raw_fd(), FBIOGATTR as _, &mut gattr) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }

        let ft = gattr.fbtype;
        if ft.fb_width <= 0 || ft.fb_height <= 0 || ft.fb_size <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("implausible fbtype: {ft:?}"),
            ));
        }

        // gfxfb_info rides in sattr.dev_specific; read it unaligned-safe.
        let info: GfxfbInfo = unsafe {
            std::ptr::read_unaligned(gattr.sattr.dev_specific.as_ptr() as *const GfxfbInfo)
        };

        let bytespp = (ft.fb_depth as u32).div_ceil(8);
        let pitch = if info.pitch >= ft.fb_width as u32 * bytespp {
            info.pitch
        } else {
            ft.fb_width as u32 * bytespp
        };

        let mode = FbMode {
            width: ft.fb_width as u32,
            height: ft.fb_height as u32,
            depth: ft.fb_depth as u32,
            pitch,
            red: (info.red_mask_size, info.red_field_position),
            green: (info.green_mask_size, info.green_field_position),
            blue: (info.blue_mask_size, info.blue_field_position),
            size: ft.fb_size as usize,
        };

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mode.size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            file,
            mode,
            base: base as *mut u8,
            graphics: false,
        })
    }

    pub fn mode(&self) -> FbMode {
        self.mode
    }

    /// Take the console: KDSETMODE(KD_GRAPHICS). The terminal stops drawing.
    pub fn set_graphics(&mut self) -> io::Result<()> {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), KDSETMODE as _, KD_GRAPHICS) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        self.graphics = true;
        Ok(())
    }

    /// Hand the console back: KDSETMODE(KD_TEXT). Called by Drop too.
    pub fn set_text(&mut self) -> io::Result<()> {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), KDSETMODE as _, KD_TEXT) };
        self.graphics = false;
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Blit a full frame of tightly-packed XRGB8888 pixels (width*height u32s)
    /// into the framebuffer, honoring the device pitch row by row — same
    /// full-frame row blit as D3 (damage tracking stays on the polish list).
    pub fn present(&mut self, pixels: &[u32]) {
        let w = self.mode.width as usize;
        let h = self.mode.height as usize;
        assert!(pixels.len() >= w * h, "frame smaller than mode");
        assert!(
            self.mode.pitch as usize * h <= self.mode.size,
            "pitch*height exceeds mapping"
        );
        let row_bytes = w * 4;
        for y in 0..h {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(y * w) as *const u8,
                    self.base.add(y * self.mode.pitch as usize),
                    row_bytes,
                );
            }
        }
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        if self.graphics {
            let _ = self.set_text();
        }
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.mode.size);
        }
    }
}

pub mod hid2evdev;
pub mod input;
