// SPDX-License-Identifier: GPL-3.0-only

//! illumos console framebuffer backend (NoctOS).
//!
//! There is no DRM/KMS on illumos, so this backend renders with GL through a
//! surfaceless EGL display into an offscreen renderbuffer, reads the frame
//! back and blits it to the console framebuffer (`/dev/fb`). Keyboard and
//! pointer come from the console's VUID streams (`/dev/kbd`, `/dev/mouse`).
//! Both device layers live in the `illumosfb` crate.
//!
//! Environment:
//! - `COSMIC_ILLUMOSFB_HEADLESS=WxH`: do not open any device; render at the
//!   given size. For testing the render path on any OS with surfaceless EGL.
//! - `COSMIC_ILLUMOSFB_DUMP=/path/frame.ppm`: also write every presented
//!   frame to this file (overwritten each time).
//!
//! Limits: one output in the mode the console booted in; software-timed 60 Hz
//! with no vsync; a full-frame readback and blit per presented frame; input
//! devices are polled on the frame timer.

use crate::{
    backend::render::{self, CursorMode, ScreenFilterStorage, init_shaders},
    config::ScreenFilter,
    input::InputBackendId,
    shell::SeatExt,
    state::{BackendData, Common, State},
};
use ::illumosfb::{
    Framebuffer,
    input::{Event as FbEvent, Keyboard, Pointer},
};
use anyhow::{Context, Result, anyhow};
use calloop::timer::{TimeoutAction, Timer};
use cosmic_comp_config::output::comp::OutputConfig;
use smithay::{
    backend::{
        allocator::Fourcc,
        egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
        input::{
            AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState, Device,
            DeviceCapability, Event, InputBackend, InputEvent, InputTime, KeyState,
            KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
            PointerMotionAbsoluteEvent, PointerMotionEvent, UnusedEvent,
        },
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::{OutputDamageTracker, RenderOutputResult},
            gles::GlesRenderbuffer,
            glow::GlowRenderer,
        },
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::DisplayHandle,
    },
    utils::{Buffer as BufferCoords, Rectangle, Size, Transform},
    wayland::presentation::Refresh,
};
use std::{
    borrow::BorrowMut,
    cell::RefCell,
    fmt,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tracing::{error, info, warn};

/// Frame and input-poll period. There is no vblank to wait for.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const REFRESH_MHZ: i32 = 60_000;

/// Linux evdev codes used for the emergency exit chord.
const KEY_BACKSPACE: u16 = 14;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;

/// Set from the SIGTERM/SIGINT handler; polled on the frame timer so the
/// console is handed back to text mode through the normal shutdown path.
static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_terminate(_signal: libc::c_int) {
    TERMINATE.store(true, Ordering::SeqCst);
}

pub struct IllumosfbState {
    pub renderer: GlowRenderer,
    output: Output,
    damage_tracker: OutputDamageTracker,
    screen_filter_state: ScreenFilterStorage,
    target: GlesRenderbuffer,
    size: Size<i32, BufferCoords>,
    /// Frames since the offscreen buffer was last fully drawn (0 = never).
    age: usize,
    /// Something changed since the last presented frame.
    dirty: bool,
    /// Console framebuffer; `None` in headless mode.
    fb: Option<Framebuffer>,
    /// Last presented frame as tightly packed XRGB8888.
    pixels: Vec<u32>,
    dump_path: Option<PathBuf>,
}

impl fmt::Debug for IllumosfbState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IllumosfbState")
            .field("output", &self.output.name())
            .field("size", &self.size)
            .field("headless", &self.fb.is_none())
            .finish_non_exhaustive()
    }
}

impl IllumosfbState {
    pub fn schedule_render(&mut self, _output: &Output) {
        self.dirty = true;
    }

    pub fn all_outputs(&self) -> Vec<Output> {
        vec![self.output.clone()]
    }

    pub fn apply_config_for_outputs(&mut self, test_only: bool) -> Result<()> {
        // The console framebuffer has exactly one mode: the one it booted in.
        let mut config = self
            .output
            .user_data()
            .get::<RefCell<OutputConfig>>()
            .unwrap()
            .borrow_mut();
        if config.mode.0 != (self.size.w, self.size.h) {
            if !test_only {
                config.mode = ((self.size.w, self.size.h), None);
            }
            Err(anyhow!("The console framebuffer mode cannot be changed"))
        } else {
            Ok(())
        }
    }

    pub fn update_screen_filter(&mut self, screen_filter: &ScreenFilter) -> Result<()> {
        self.screen_filter_state.filter = screen_filter.clone();
        self.dirty = true;
        Ok(())
    }

    #[profiling::function]
    fn render_output(&mut self, state: &mut Common) -> Result<()> {
        let timing = std::env::var_os("COSMIC_ILLUMOSFB_TIMING").is_some();
        let t_start = std::time::Instant::now();
        let (mut t_render, mut t_read, mut t_convert) = (t_start, t_start, t_start);
        let mut fb = self
            .renderer
            .bind(&mut self.target)
            .with_context(|| "Failed to bind offscreen buffer")?;
        match render::render_output(
            None,
            &mut self.renderer,
            &mut fb,
            &mut self.damage_tracker,
            self.age,
            &state.shell,
            state.clock.now(),
            &self.output,
            CursorMode::All,
            &mut self.screen_filter_state,
            &state.event_loop_handle,
        ) {
            Ok(RenderOutputResult { damage, states, .. }) => {
                let presented = damage.is_some();
                t_render = std::time::Instant::now();
                t_read = t_render;
                t_convert = t_render;
                if presented {
                    // GL_RGBA readback is the one format every GLES
                    // implementation must support.
                    let mapping = self
                        .renderer
                        .copy_framebuffer(&fb, Rectangle::from_size(self.size), Fourcc::Abgr8888)
                        .with_context(|| "Failed to read back the rendered frame")?;
                    let bytes = self
                        .renderer
                        .map_texture(&mapping)
                        .with_context(|| "Failed to map the rendered frame")?;
                    t_read = std::time::Instant::now();
                    rgba_to_xrgb(bytes, &mut self.pixels, self.size);
                    t_convert = std::time::Instant::now();
                }
                std::mem::drop(fb);
                self.age = 1;

                if presented {
                    if let Some(device) = self.fb.as_mut() {
                        device.present(&self.pixels);
                    }
                    if let Some(path) = self.dump_path.as_ref()
                        && let Err(err) = write_ppm(path, &self.pixels, self.size)
                    {
                        warn!(?err, "Failed to dump frame to {}", path.display());
                    }
                }
                if timing && presented {
                    let ms = |a: std::time::Instant, b: std::time::Instant| {
                        b.duration_since(a).as_secs_f64() * 1000.0
                    };
                    eprintln!(
                        "illumosfb frame: render {:.1} ms, readback {:.1} ms, convert {:.1} ms, present {:.1} ms",
                        ms(t_start, t_render),
                        ms(t_render, t_read),
                        ms(t_read, t_convert),
                        ms(t_convert, std::time::Instant::now()),
                    );
                }

                state.send_frames(&self.output, None);
                state.update_primary_output(&self.output, &states);
                state.send_dmabuf_feedback(&self.output, &states, |_| None);
                if presented {
                    let mut output_presentation_feedback = state
                        .shell
                        .read()
                        .take_presentation_feedback(&self.output, &states);
                    output_presentation_feedback.presented(
                        state.clock.now(),
                        Refresh::Fixed(FRAME_INTERVAL),
                        0,
                        wp_presentation_feedback::Kind::empty(),
                    );
                }
            }
            Err(err) => {
                // Redraw everything next time.
                self.age = 0;
                anyhow::bail!("Rendering failed: {}", err);
            }
        };

        Ok(())
    }
}

/// Convert a GL_RGBA readback (bytes R,G,B,A) into XRGB8888 words. The
/// renderer draws offscreen targets so that the readback is already top-down
/// (verified by dumping a frame: a flip here puts the cursor at the bottom).
fn rgba_to_xrgb(src: &[u8], dst: &mut [u32], size: Size<i32, BufferCoords>) {
    let count = (size.w * size.h) as usize;
    debug_assert!(src.len() >= count * 4 && dst.len() >= count);
    for (px, rgba) in dst[..count].iter_mut().zip(src.chunks_exact(4)) {
        *px = (rgba[0] as u32) << 16 | (rgba[1] as u32) << 8 | rgba[2] as u32;
    }
}

fn write_ppm(path: &PathBuf, pixels: &[u32], size: Size<i32, BufferCoords>) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    write!(out, "P6\n{} {}\n255\n", size.w, size.h)?;
    for px in pixels {
        out.write_all(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8])?;
    }
    out.flush()?;
    drop(out);
    std::fs::rename(tmp, path)
}

fn headless_size() -> Result<Option<(i32, i32)>> {
    let Some(value) = std::env::var_os("COSMIC_ILLUMOSFB_HEADLESS") else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    let parsed = value
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
    match parsed {
        Some((w, h)) if w > 0 && h > 0 => Ok(Some((w, h))),
        _ => Err(anyhow!(
            "COSMIC_ILLUMOSFB_HEADLESS must look like 1280x800, got {value:?}"
        )),
    }
}

pub fn init_backend(
    _dh: &DisplayHandle,
    event_loop: &mut EventLoop<State>,
    state: &mut State,
) -> Result<()> {
    // Renderer first: if GL is unavailable, fail before touching the console.
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }
        .with_context(|| "Failed to create a surfaceless EGL display")?;
    let context = EGLContext::new(&display).with_context(|| "Failed to create an EGL context")?;
    let mut renderer =
        unsafe { GlowRenderer::new(context) }.with_context(|| "Failed to create the renderer")?;
    init_shaders(renderer.borrow_mut()).context("Failed to initialize renderer")?;

    let headless = headless_size()?;
    let mut fb = match headless {
        Some(_) => None,
        None => Some(
            Framebuffer::open("/dev/fb").with_context(|| "Failed to open /dev/fb")?,
        ),
    };
    let (width, height) = match (&fb, headless) {
        (Some(fb), _) => {
            let mode = fb.mode();
            if !mode.is_xrgb8888() {
                return Err(anyhow!(
                    "Unsupported console framebuffer format: {mode:?} (need 32-bit XRGB)"
                ));
            }
            (mode.width as i32, mode.height as i32)
        }
        (None, Some(size)) => size,
        (None, None) => unreachable!(),
    };
    let size: Size<i32, BufferCoords> = (width, height).into();
    info!(width, height, headless = fb.is_none(), "illumosfb output");

    let target: GlesRenderbuffer = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .with_context(|| "Failed to create the offscreen render buffer")?;

    let name = "FB-0".to_string();
    let props = PhysicalProperties {
        size: (0, 0).into(),
        subpixel: Subpixel::Unknown,
        make: "NoctOS".to_string(),
        model: "illumos console framebuffer".to_string(),
        serial_number: "Unknown".to_string(),
    };
    let mode = Mode {
        size: (width, height).into(),
        refresh: REFRESH_MHZ,
    };
    let output = Output::new(name, props);
    output.add_mode(mode);
    output.set_preferred(mode);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.user_data().insert_if_missing(|| {
        RefCell::new(OutputConfig {
            mode: ((width, height), None),
            ..Default::default()
        })
    });

    // Input devices. Opened before the console switches to graphics so a
    // failure here leaves a usable text console.
    let mut keyboard = None;
    let mut pointer = None;
    if fb.is_some() {
        keyboard = Some(Keyboard::open("/dev/kbd").with_context(|| "Failed to open /dev/kbd")?);
        pointer = match Pointer::open("/dev/mouse", width as u32, height as u32) {
            Ok(pointer) => Some(pointer),
            Err(err) => {
                warn!(?err, "Failed to open /dev/mouse; continuing without a pointer");
                None
            }
        };
    }

    // SIGTERM/SIGINT must go through the normal shutdown so the console
    // returns to text mode and the keyboard to its previous translation.
    unsafe {
        libc::signal(libc::SIGTERM, on_terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_terminate as *const () as libc::sighandler_t);
    }

    if let Some(fb) = fb.as_mut() {
        fb.set_graphics()
            .with_context(|| "Failed to switch the console to graphics mode")?;
    }

    let dump_path = std::env::var_os("COSMIC_ILLUMOSFB_DUMP").map(PathBuf::from);

    state.backend = BackendData::Illumosfb(IllumosfbState {
        renderer,
        damage_tracker: OutputDamageTracker::from_output(&output),
        output: output.clone(),
        screen_filter_state: ScreenFilterStorage::default(),
        target,
        size,
        age: 0,
        dirty: true,
        fb,
        pixels: vec![0; (width * height) as usize],
        dump_path,
    });

    let mut input = InputPump {
        keyboard,
        pointer,
        announced: false,
        keys_down: 0,
        ctrl: false,
        alt: false,
        fb_size: (width as f64, height as f64),
        events: Vec::new(),
    };

    event_loop
        .handle()
        .insert_source(Timer::from_duration(FRAME_INTERVAL), move |_, _, state| {
            if TERMINATE.load(Ordering::SeqCst) {
                state.common.should_stop = true;
                return TimeoutAction::Drop;
            }

            if input.dispatch(state) {
                state.backend.illumosfb().dirty = true;
            }

            let animating = state.common.shell.read().animations_going();
            let fb_state = state.backend.illumosfb();
            if fb_state.dirty || animating {
                fb_state.dirty = false;
                if let Err(err) = fb_state.render_output(&mut state.common) {
                    error!(?err, "Failed to render frame.");
                    state.backend.illumosfb().dirty = true;
                }
                profiling::finish_frame!();
            }
            TimeoutAction::ToDuration(FRAME_INTERVAL)
        })
        .map_err(|_| anyhow!("Failed to init the frame timer for illumosfb"))?;

    state
        .common
        .output_configuration_state
        .add_heads(std::iter::once(&output));
    {
        state.common.add_output(&output);
        if let Err(err) = state.common.config.read_outputs(
            &mut state.common.output_configuration_state,
            &mut state.backend,
            &state.common.shell,
            &state.common.event_loop_handle,
            &mut state.common.workspace_state.update(),
            &state.common.xdg_activation_state,
            state.common.startup_done.clone(),
            &state.common.clock,
        ) {
            error!("Unrecoverable output config error: {}", err);
        }
        state.common.refresh();
    }

    if state.common.with_xwayland {
        state.launch_xwayland(None);
    } else {
        state.notify_ready();
    }

    Ok(())
}

/// Polls the console input devices and feeds the compositor.
struct InputPump {
    keyboard: Option<Keyboard>,
    pointer: Option<Pointer>,
    announced: bool,
    keys_down: u32,
    ctrl: bool,
    alt: bool,
    fb_size: (f64, f64),
    events: Vec<FbEvent>,
}

impl InputPump {
    /// Returns whether any event was delivered.
    fn dispatch(&mut self, state: &mut State) -> bool {
        if self.keyboard.is_none() && self.pointer.is_none() {
            return false;
        }

        // The seat is created after the backend is initialized, so the device
        // is announced on the first tick, not during init.
        if !self.announced {
            self.announced = true;
            state.process_input_event(
                InputEvent::<IllumosfbInput>::DeviceAdded {
                    device: IllumosfbDevice,
                },
                InputBackendId::Normal,
            );
            let output = state.backend.illumosfb().output.clone();
            for seat in state.common.shell.read().seats.iter() {
                seat.set_active_output(&output);
            }
        }

        self.events.clear();
        if let Some(keyboard) = self.keyboard.as_mut()
            && let Err(err) = keyboard.poll(&mut self.events)
        {
            warn!(?err, "Failed to read /dev/kbd");
        }
        if let Some(pointer) = self.pointer.as_mut()
            && let Err(err) = pointer.poll(&mut self.events)
        {
            warn!(?err, "Failed to read /dev/mouse");
        }
        if self.events.is_empty() {
            return false;
        }

        let events = std::mem::take(&mut self.events);
        for event in &events {
            let time = InputTime::now();
            match *event {
                FbEvent::Key { code, pressed } => {
                    match code {
                        KEY_LEFTCTRL | KEY_RIGHTCTRL => self.ctrl = pressed,
                        KEY_LEFTALT | KEY_RIGHTALT => self.alt = pressed,
                        _ => {}
                    }
                    // Emergency exit: Ctrl+Alt+Backspace always shuts down,
                    // whatever the compositor's own key bindings are.
                    if code == KEY_BACKSPACE && pressed && self.ctrl && self.alt {
                        info!("Ctrl+Alt+Backspace: shutting down");
                        state.common.should_stop = true;
                        continue;
                    }
                    if pressed {
                        self.keys_down += 1;
                    } else {
                        self.keys_down = self.keys_down.saturating_sub(1);
                    }
                    state.process_input_event(
                        InputEvent::<IllumosfbInput>::Keyboard {
                            event: KeyEvent {
                                time,
                                code,
                                pressed,
                                count: self.keys_down,
                            },
                        },
                        InputBackendId::Normal,
                    );
                }
                FbEvent::UnknownKey { .. } => {}
                FbEvent::Button { code, pressed } => state.process_input_event(
                    InputEvent::<IllumosfbInput>::PointerButton {
                        event: ButtonEvent {
                            time,
                            code,
                            pressed,
                        },
                    },
                    InputBackendId::Normal,
                ),
                FbEvent::Motion { dx, dy } => state.process_input_event(
                    InputEvent::<IllumosfbInput>::PointerMotion {
                        event: MotionEvent {
                            time,
                            dx: dx as f64,
                            dy: dy as f64,
                        },
                    },
                    InputBackendId::Normal,
                ),
                FbEvent::MotionAbsolute { x, y } => state.process_input_event(
                    InputEvent::<IllumosfbInput>::PointerMotionAbsolute {
                        event: MotionAbsoluteEvent {
                            time,
                            x: x as f64,
                            y: y as f64,
                            fb_size: self.fb_size,
                        },
                    },
                    InputBackendId::Normal,
                ),
                FbEvent::Wheel { wheel, delta } => state.process_input_event(
                    InputEvent::<IllumosfbInput>::PointerAxis {
                        event: WheelEvent {
                            time,
                            horizontal: wheel != 0,
                            delta,
                        },
                    },
                    InputBackendId::Normal,
                ),
            }
        }
        self.events = events;
        true
    }
}

/// Marker type defining the `InputBackend` types for this backend.
#[derive(Debug)]
pub struct IllumosfbInput;

/// The console keyboard and mouse streams, presented as one device.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct IllumosfbDevice;

impl Device for IllumosfbDevice {
    fn id(&self) -> String {
        String::from("illumos-console")
    }

    fn name(&self) -> String {
        String::from("illumos console keyboard and mouse")
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(
            capability,
            DeviceCapability::Keyboard | DeviceCapability::Pointer
        )
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

macro_rules! impl_event {
    ($ty:ty) => {
        impl Event<IllumosfbInput> for $ty {
            fn time(&self) -> InputTime {
                self.time
            }

            fn device(&self) -> IllumosfbDevice {
                IllumosfbDevice
            }
        }
    };
}

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    time: InputTime,
    /// Linux evdev key code.
    code: u16,
    pressed: bool,
    count: u32,
}
impl_event!(KeyEvent);

impl KeyboardKeyEvent<IllumosfbInput> for KeyEvent {
    fn key_code(&self) -> Keycode {
        // xkb key codes are evdev codes offset by 8.
        (self.code as u32 + 8).into()
    }

    fn state(&self) -> KeyState {
        if self.pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        }
    }

    fn count(&self) -> u32 {
        self.count
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ButtonEvent {
    time: InputTime,
    /// Linux evdev button code (BTN_LEFT = 0x110, ...).
    code: u16,
    pressed: bool,
}
impl_event!(ButtonEvent);

impl PointerButtonEvent<IllumosfbInput> for ButtonEvent {
    fn button_code(&self) -> u32 {
        self.code as u32
    }

    fn state(&self) -> ButtonState {
        if self.pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MotionEvent {
    time: InputTime,
    dx: f64,
    dy: f64,
}
impl_event!(MotionEvent);

impl PointerMotionEvent<IllumosfbInput> for MotionEvent {
    fn delta_x(&self) -> f64 {
        self.dx
    }

    fn delta_y(&self) -> f64 {
        self.dy
    }

    fn delta_x_unaccel(&self) -> f64 {
        self.dx
    }

    fn delta_y_unaccel(&self) -> f64 {
        self.dy
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MotionAbsoluteEvent {
    time: InputTime,
    /// Position in framebuffer pixels (the mouse driver is told the screen
    /// size, so absolute devices report in these units).
    x: f64,
    y: f64,
    fb_size: (f64, f64),
}
impl_event!(MotionAbsoluteEvent);

impl PointerMotionAbsoluteEvent<IllumosfbInput> for MotionAbsoluteEvent {}
impl AbsolutePositionEvent<IllumosfbInput> for MotionAbsoluteEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, width: i32) -> f64 {
        (self.x / self.fb_size.0 * width as f64).max(0.0)
    }

    fn y_transformed(&self, height: i32) -> f64 {
        (self.y / self.fb_size.1 * height as f64).max(0.0)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WheelEvent {
    time: InputTime,
    horizontal: bool,
    /// Wheel clicks; positive is away from the user.
    delta: i8,
}
impl_event!(WheelEvent);

impl PointerAxisEvent<IllumosfbInput> for WheelEvent {
    fn amount(&self, _axis: Axis) -> Option<f64> {
        None
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        let ours = if self.horizontal {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        // Wayland's vertical axis is positive towards the user.
        (axis == ours).then(|| -(self.delta as f64) * 120.0)
    }

    fn source(&self) -> AxisSource {
        AxisSource::Wheel
    }

    fn relative_direction(&self, _axis: Axis) -> AxisRelativeDirection {
        AxisRelativeDirection::Identical
    }
}

impl InputBackend for IllumosfbInput {
    type Device = IllumosfbDevice;
    type KeyboardKeyEvent = KeyEvent;
    type PointerAxisEvent = WheelEvent;
    type PointerButtonEvent = ButtonEvent;
    type PointerMotionEvent = MotionEvent;
    type PointerMotionAbsoluteEvent = MotionAbsoluteEvent;

    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;

    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}
