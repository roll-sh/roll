//! Milliseconds per transformation, one core (what one request costs):
//!
//! ```text
//! cargo run --release -p s3-image --example bench [image files…]
//! ```
//!
//! Without files it makes photo-like JPEG sources at typical upload sizes (1, 3 and 12 MP). Each
//! case runs a few times; the median is reported, as the thread's CPU time (what a transform
//! costs, unaffected by other load on the machine) and wall time, with the output's size.

use s3_image::{Limits, Params, Rendered, transform};
use std::time::Instant;

/// This thread's CPU time, in ms.
fn cpu_ms() -> f64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid out-pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
    t.tv_sec as f64 * 1000.0 + t.tv_nsec as f64 / 1e6
}

/// Gradients plus fine noise: compresses like a photo (unlike flat test patterns).
fn photo_jpeg(w: u32, h: u32) -> Vec<u8> {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let n = (seed % 24) as i32 - 12;
            let r = (x * 255 / w) as i32 + n;
            let g = (y * 255 / h) as i32 + n;
            let b = (((x / 40 + y / 40) % 2) * 120 + 60) as i32 + n;
            px.extend([r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8]);
        }
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 88).encode(&px, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).unwrap();
    out
}

fn main() {
    let mut sources: Vec<(String, Vec<u8>)> = std::env::args().skip(1).map(|p| (p.clone(), std::fs::read(&p).unwrap())).collect();
    if sources.is_empty() {
        for (w, h) in [(1200, 800), (2048, 1536), (4000, 3000)] {
            sources.push((format!("synthetic {w}x{h} JPEG"), photo_jpeg(w, h)));
        }
    }
    let limits = Limits::default();
    let cases = [
        "width:300,format:jpeg",
        "width:300,format:webp",
        "width:300,format:avif",
        "width:1200,format:jpeg",
        "width:1200,format:webp",
        "width:1200,format:avif",
        "width:300,height:300,resize:cover,format:webp",
        "format:origin",
    ];
    println!("{:<34} {:<46} {:>9} {:>9} {:>10}", "source", "transformation", "cpu ms", "wall ms", "bytes");
    for (name, input) in &sources {
        println!("{name}: {} KiB", input.len() / 1024);
        for spec in cases {
            let plan = Params::parse_spec(spec).unwrap().resolve(None);
            let runs = if spec.contains("avif") && !spec.contains("300") { 3 } else { 5 };
            let (mut times, mut cpu) = (Vec::new(), Vec::new());
            let mut size = 0;
            for _ in 0..runs {
                let (t, c) = (Instant::now(), cpu_ms());
                let r = transform(input, &plan, &limits).unwrap();
                cpu.push(cpu_ms() - c);
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                if let Rendered::Image { bytes, .. } = r {
                    size = bytes.len();
                }
            }
            times.sort_by(f64::total_cmp);
            cpu.sort_by(f64::total_cmp);
            println!("{:<34} {:<46} {:>9.1} {:>9.1} {:>10}", "", spec, cpu[cpu.len() / 2], times[times.len() / 2], size);
        }
    }
}
