//! fbdemo — S1 proof: the VM console painted from Rust.
//!
//! Opens /dev/fb, reports the mode, switches to graphics, animates ~6 seconds
//! (gradient background + bouncing square + sweep bar), restores text mode.
//! Run it like tinywl/fbtest: from a console login (logindevperm owns /dev/fb)
//! or under pfexec. If it's ever SIGKILLed mid-run the console stays graphic;
//! rerun it (or any KD_TEXT setter) to recover.

use illumosfb::Framebuffer;
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/dev/fb".into());
    let mut fb = match Framebuffer::open(&path) {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("fbdemo: {path}: {e}");
            eprintln!("fbdemo: run from a console login or under pfexec");
            std::process::exit(1);
        }
    };

    let m = fb.mode();
    println!(
        "fbdemo: {}x{} depth {} pitch {} size {}  R{}/{} G{}/{} B{}/{}  xrgb8888={}",
        m.width, m.height, m.depth, m.pitch, m.size,
        m.red.0, m.red.1, m.green.0, m.green.1, m.blue.0, m.blue.1,
        m.is_xrgb8888()
    );
    if m.depth != 32 {
        eprintln!("fbdemo: only 32bpp handled (got {})", m.depth);
        std::process::exit(1);
    }

    let (w, h) = (m.width as usize, m.height as usize);
    let mut frame = vec![0u32; w * h];

    println!("fbdemo: entering graphics mode for ~6s...");
    fb.set_graphics().expect("KDSETMODE(KD_GRAPHICS)");

    let start = Instant::now();
    let dur = Duration::from_secs(6);
    // Bouncing square state.
    let side = (h / 6).max(32);
    let (mut bx, mut by) = (40i64, 40i64);
    let (mut vx, mut vy) = (7i64, 5i64);

    while start.elapsed() < dur {
        let t = start.elapsed().as_secs_f32();

        // Background: static two-axis gradient (blue by x, green by y).
        for y in 0..h {
            let g = (y * 255 / h) as u32;
            let row = &mut frame[y * w..(y + 1) * w];
            for (x, px) in row.iter_mut().enumerate() {
                let b = (x * 255 / w) as u32;
                *px = (g << 8) | b;
            }
        }

        // Sweep bar: a white vertical bar crossing the screen each 2s.
        let bar_x = ((t % 2.0) / 2.0 * w as f32) as usize;
        let bar_w = 8.min(w - bar_x);
        for y in 0..h {
            for px in &mut frame[y * w + bar_x..y * w + bar_x + bar_w] {
                *px = 0x00ff_ffff;
            }
        }

        // Bouncing red square.
        bx += vx;
        by += vy;
        if bx < 0 || bx + side as i64 >= w as i64 {
            vx = -vx;
            bx += vx;
        }
        if by < 0 || by + side as i64 >= h as i64 {
            vy = -vy;
            by += vy;
        }
        for y in by as usize..(by as usize + side).min(h) {
            for px in &mut frame[y * w + bx as usize..y * w + (bx as usize + side).min(w)] {
                *px = 0x00ff_2020;
            }
        }

        fb.present(&frame);
        std::thread::sleep(Duration::from_millis(16)); // ~60 Hz, D3-style timer pace
    }

    drop(fb); // KD_TEXT + munmap
    println!("fbdemo: done — text mode restored");
}
