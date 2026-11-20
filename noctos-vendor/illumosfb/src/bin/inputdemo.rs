//! inputdemo — S2 proof: VUID keyboard + pointer decoded to evdev in Rust.
//!
//! Prints decoded events from /dev/kbd and /dev/mouse. While /dev/kbd is in
//! direct+untranslated mode your console shell gets no keystrokes, so this
//! runs for 15 seconds (or until you press ESC) and then restores the
//! keyboard. Run from a console login or under pfexec, like fbdemo.

use illumosfb::input::{Event, Keyboard, Pointer};
use std::time::{Duration, Instant};

const KEY_ESC: u16 = 1;

fn main() {
    let (w, h) = (1280, 800); // only used to scale absolute devices
    let mut kbd = match Keyboard::open("/dev/kbd") {
        Ok(k) => k,
        Err(e) => {
            eprintln!("inputdemo: /dev/kbd: {e} (console login or pfexec)");
            std::process::exit(1);
        }
    };
    let mut ptr = match Pointer::open("/dev/mouse", w, h) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("inputdemo: /dev/mouse: {e} — keyboard only");
            None
        }
    };

    println!("inputdemo: type keys / move mouse. ESC or 15s to exit.");
    let start = Instant::now();
    let mut events = Vec::new();
    'outer: while start.elapsed() < Duration::from_secs(15) {
        events.clear();
        kbd.poll(&mut events).expect("kbd poll");
        if let Some(p) = ptr.as_mut() {
            p.poll(&mut events).expect("mouse poll");
        }
        for ev in &events {
            match *ev {
                Event::Key { code, pressed } => {
                    println!("key   evdev {code:3} {}", if pressed { "down" } else { "up" });
                    if code == KEY_ESC && !pressed {
                        break 'outer;
                    }
                }
                Event::UnknownKey { usage, pressed } => {
                    println!("key   hid {usage:#04x} (unmapped) {}", if pressed { "down" } else { "up" })
                }
                Event::Button { code, pressed } => {
                    println!("btn   {code:#05x} {}", if pressed { "down" } else { "up" })
                }
                Event::Motion { dx, dy } => println!("rel   dx {dx:4} dy {dy:4}"),
                Event::MotionAbsolute { x, y } => println!("abs   x {x:5} y {y:5}"),
                Event::Wheel { wheel, delta } => println!("wheel #{wheel} delta {delta}"),
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(kbd); // restore translation + direct off
    println!("inputdemo: done — keyboard restored");
}
