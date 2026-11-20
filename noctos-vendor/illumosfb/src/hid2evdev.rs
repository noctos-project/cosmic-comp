//! USB HID usage (keyboard page 0x07) -> Linux evdev keycode.
//!
//! Mechanical transcription of FreeBSD's evdev_usb_scancodes table
//! (sys/dev/evdev/evdev_utils.c @ a0a11b87f4f1, BSD-2-Clause, values
//! UNMODIFIED) -- the same table the D3 C backend vendors verbatim.
//! Valid because conskbd forces KB_USB into every linked keyboard
//! driver, so TR_UNTRANS_EVENT firm-event ids ARE USB HID usages.

/// evdev keycode for a HID usage id, 0 = none.
pub const EVDEV_USB_SCANCODES: [u16; 256] = [
      0,   0,   0,   0,  30,  48,  46,  32, // 0x00: KEY_A KEY_B KEY_C KEY_D
     18,  33,  34,  35,  23,  36,  37,  38, // 0x08: KEY_E KEY_F KEY_G KEY_H KEY_I KEY_J KEY_K KEY_L
     50,  49,  24,  25,  16,  19,  31,  20, // 0x10: KEY_M KEY_N KEY_O KEY_P KEY_Q KEY_R KEY_S KEY_T
     22,  47,  17,  45,  21,  44,   2,   3, // 0x18: KEY_U KEY_V KEY_W KEY_X KEY_Y KEY_Z KEY_1 KEY_2
      4,   5,   6,   7,   8,   9,  10,  11, // 0x20: KEY_3 KEY_4 KEY_5 KEY_6 KEY_7 KEY_8 KEY_9 KEY_0
     28,   1,  14,  15,  57,  12,  13,  26, // 0x28: KEY_ENTER KEY_ESC KEY_BACKSPACE KEY_TAB KEY_SPACE KEY_MINUS KEY_EQUAL KEY_LEFTBRACE
     27,  43,  43,  39,  40,  41,  51,  52, // 0x30: KEY_RIGHTBRACE KEY_BACKSLASH KEY_BACKSLASH KEY_SEMICOLON KEY_APOSTROPHE KEY_GRAVE KEY_COMMA KEY_DOT
     53,  58,  59,  60,  61,  62,  63,  64, // 0x38: KEY_SLASH KEY_CAPSLOCK KEY_F1 KEY_F2 KEY_F3 KEY_F4 KEY_F5 KEY_F6
     65,  66,  67,  68,  87,  88,  99,  70, // 0x40: KEY_F7 KEY_F8 KEY_F9 KEY_F10 KEY_F11 KEY_F12 KEY_SYSRQ KEY_SCROLLLOCK
    119, 110, 102, 104, 111, 107, 109, 106, // 0x48: KEY_PAUSE KEY_INSERT KEY_HOME KEY_PAGEUP KEY_DELETE KEY_END KEY_PAGEDOWN KEY_RIGHT
    105, 108, 103,  69,  98,  55,  74,  78, // 0x50: KEY_LEFT KEY_DOWN KEY_UP KEY_NUMLOCK KEY_KPSLASH KEY_KPASTERISK KEY_KPMINUS KEY_KPPLUS
     96,  79,  80,  81,  75,  76,  77,  71, // 0x58: KEY_KPENTER KEY_KP1 KEY_KP2 KEY_KP3 KEY_KP4 KEY_KP5 KEY_KP6 KEY_KP7
     72,  73,  82,  83,  86, 127, 116, 117, // 0x60: KEY_KP8 KEY_KP9 KEY_KP0 KEY_KPDOT KEY_102ND KEY_COMPOSE KEY_POWER KEY_KPEQUAL
    183, 184, 185, 186, 187, 188, 189, 190, // 0x68: KEY_F13 KEY_F14 KEY_F15 KEY_F16 KEY_F17 KEY_F18 KEY_F19 KEY_F20
    191, 192, 193, 194, 134, 138, 130, 132, // 0x70: KEY_F21 KEY_F22 KEY_F23 KEY_F24 KEY_OPEN KEY_HELP KEY_PROPS KEY_FRONT
    128, 129, 131, 137, 133, 135, 136, 113, // 0x78: KEY_STOP KEY_AGAIN KEY_UNDO KEY_CUT KEY_COPY KEY_PASTE KEY_FIND KEY_MUTE
    115, 114,   0,   0,   0, 121,   0,  89, // 0x80: KEY_VOLUMEUP KEY_VOLUMEDOWN KEY_KPCOMMA KEY_RO
     93, 124,  92,  94,  95,   0,   0,   0, // 0x88: KEY_KATAKANAHIRAGANA KEY_YEN KEY_HENKAN KEY_MUHENKAN KEY_KPJPCOMMA
    122, 123,  90,  91,  85,   0,   0,   0, // 0x90: KEY_HANGEUL KEY_HANJA KEY_KATAKANA KEY_HIRAGANA KEY_ZENKAKUHANKAKU
      0,   0,   0,   0,   0,   0,   0,   0, // 0x98: NONE
      0,   0,   0,   0,   0,   0,   0,   0, // 0xa0: NONE
      0,   0,   0,   0,   0,   0,   0,   0, // 0xa8: NONE
      0,   0,   0,   0,   0,   0,   0,   0, // 0xb0: NONE
      0,   0,   0,   0,   0,   0,   0,   0, // 0xb8: NONE
    224, 225, 120, 204, 229, 230,   0,   0, // 0xc0: KEY_BRIGHTNESSDOWN KEY_BRIGHTNESSUP KEY_SCALE KEY_DASHBOARD KEY_KBDILLUMDOWN KEY_KBDILLUMUP
      0,   0,   0,   0,   0,   0,   0,   0, // 0xc8: NONE
      0,   0,   0,   0,   0,   0,   0,   0, // 0xd0: NONE
      0,   0,   0,   0,   0,   0,   0,   0, // 0xd8: NONE
     29,  42,  56, 125,  97,  54, 100, 126, // 0xe0: KEY_LEFTCTRL KEY_LEFTSHIFT KEY_LEFTALT KEY_LEFTMETA KEY_RIGHTCTRL KEY_RIGHTSHIFT KEY_RIGHTALT KEY_RIGHTMETA
    164, 166, 165, 163, 161, 115, 114, 113, // 0xe8: KEY_PLAYPAUSE KEY_STOPCD KEY_PREVIOUSSONG KEY_NEXTSONG KEY_EJECTCD KEY_VOLUMEUP KEY_VOLUMEDOWN KEY_MUTE
    150, 158, 159, 128, 136, 177, 178, 176, // 0xf0: KEY_WWW KEY_BACK KEY_FORWARD KEY_STOP KEY_FIND KEY_SCROLLUP KEY_SCROLLDOWN KEY_EDIT
    142, 152, 173, 140,   0,   0,   0, 143, // 0xf8: KEY_SLEEP KEY_COFFEE KEY_REFRESH KEY_CALC KEY_WAKEUP
];

/// Linux evdev button codes for VUID BUT(1..3) (left/middle/right).
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

pub fn usb_to_evdev(usage: u16) -> Option<u16> {
    let code = *EVDEV_USB_SCANCODES.get(usage as usize)?;
    (code != 0).then_some(code)
}
