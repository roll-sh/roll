//! LOLWUT: computer art and the version, like Redis 8 (TAPE MARK I verses), with VERSION 5
//! (Schotter) and 6 (a skyline); other versions just print the version line.

use super::*;
use super::server::REDIS_VERSION;
use crate::util::Rng;
use std::f64::consts::PI;

/// LOLWUT [VERSION version] [version-specific arguments…]
pub fn lolwut(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (version, rest) = if args.len() >= 3 && eq(&args[1], "version") { (int(&args[2])? as u32, &args[3..]) } else { (8, &args[1..]) };
    let _ = cx;
    let mut rng = Rng::new();
    let text = match version {
        5 => schotter_command(rest, &mut rng)?,
        6 => skyline_command(rest, &mut rng)?,
        8 => tape_mark(rest, &mut rng),
        _ => format!("Redis ver. {REDIS_VERSION}\n"),
    };
    Ok(Reply::Verbatim("txt", Bytes::from(text)))
}

/// The optional integer arguments, defaulted and clamped.
fn opts<const N: usize>(rest: &[Bytes], defaults: [i64; N], limits: [(i64, i64); N]) -> Result<[i64; N], Error> {
    let mut out = defaults;
    for (i, a) in rest.iter().take(N).enumerate() {
        out[i] = int(a)?;
    }
    for (v, (lo, hi)) in out.iter_mut().zip(limits) {
        *v = (*v).clamp(lo, hi);
    }
    Ok(out)
}

/// A random integer in `0..2^31` (C's `rand()` range).
fn rand(rng: &mut Rng) -> i64 {
    (rng.next_u64() >> 33) as i64
}

const RAND_MAX: f64 = 2147483647.0;

struct Canvas {
    width: i64,
    height: i64,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: i64, height: i64, bg: u8) -> Self {
        Self { width, height, pixels: vec![bg; (width * height) as usize] }
    }

    fn set(&mut self, x: i64, y: i64, c: u8) {
        if x >= 0 && x < self.width && y >= 0 && y < self.height {
            self.pixels[(x + y * self.width) as usize] = c;
        }
    }

    fn get(&self, x: i64, y: i64) -> u8 {
        if x >= 0 && x < self.width && y >= 0 && y < self.height { self.pixels[(x + y * self.width) as usize] } else { 0 }
    }

    /// Bresenham.
    fn line(&mut self, mut x1: i64, mut y1: i64, x2: i64, y2: i64, c: u8) {
        let (dx, dy) = ((x2 - x1).abs(), (y2 - y1).abs());
        let (sx, sy) = (if x1 < x2 { 1 } else { -1 }, if y1 < y2 { 1 } else { -1 });
        let mut err = dx - dy;
        loop {
            self.set(x1, y1, c);
            if x1 == x2 && y1 == y2 {
                break;
            }
            let e2 = err * 2;
            if e2 > -dy {
                err -= dy;
                x1 += sx;
            }
            if e2 < dx {
                err += dx;
                y1 += sy;
            }
        }
    }

    /// A square centred at x,y with the given side and rotation.
    fn square(&mut self, x: i64, y: i64, size: f32, angle: f32, c: u8) {
        // C's float arithmetic and truncated constant, so the points land on the same pixels.
        #[allow(clippy::approx_constant)]
        let size = ((size as f64 / 1.414_213_562_3) as f32 as f64).round();
        let mut k = (PI / 4.0 + angle as f64) as f32;
        let mut px = [0i64; 4];
        let mut py = [0i64; 4];
        for j in 0..4 {
            px[j] = ((k as f64).sin() * size + x as f64).round() as i64;
            py[j] = ((k as f64).cos() * size + y as f64).round() as i64;
            k = (k as f64 + PI / 2.0) as f32;
        }
        for j in 0..4 {
            self.line(px[j], py[j], px[(j + 1) % 4], py[(j + 1) % 4], c);
        }
    }

    /// Braille characters, each a 2x4 block of pixels.
    fn braille(&self) -> String {
        let mut s = String::new();
        let mut y = 0;
        while y < self.height {
            let mut x = 0;
            while x < self.width {
                let mut b = 0u32;
                for (bit, (dx, dy)) in [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)].into_iter().enumerate() {
                    if self.get(x + dx, y + dy) != 0 {
                        b |= 1 << bit;
                    }
                }
                s.push(char::from_u32(0x2800 + b).unwrap());
                x += 2;
            }
            if y != self.height - 1 {
                s.push('\n');
            }
            y += 4;
        }
        s
    }
}

/// LOLWUT VERSION 5 [columns] [squares-per-row] [squares-per-col]: Georg Nees' Schotter.
fn schotter_command(rest: &[Bytes], rng: &mut Rng) -> Result<String, Error> {
    let [cols, per_row, per_col] = opts(rest, [66, 8, 12], [(1, 1000), (1, 200), (1, 200)])?;
    let width = cols * 2;
    let padding = if width > 4 { 2 } else { 0 };
    let side = (width - padding * 2) as f32 / per_row as f32;
    let height = (side * per_col as f32) as i64 + padding * 2;
    let mut c = Canvas::new(width, height, 0);
    for y in 0..per_col {
        for x in 0..per_row {
            let mut sx = (x as f32 * side + side / 2.0) as i64 + padding;
            let mut sy = (y as f32 * side + side / 2.0) as i64 + padding;
            let mut angle = 0.0f32;
            if y > 1 {
                let mut r = [0f32; 3];
                for v in &mut r {
                    *v = (rand(rng) as f64 / RAND_MAX) as f32 / per_col as f32 * y as f32;
                }
                for v in &mut r {
                    if rand(rng) % 2 == 1 {
                        *v = -*v;
                    }
                }
                angle = r[0];
                sx = (sx as f32 + r[1] * side / 3.0) as i64;
                sy = (sy as f32 + r[2] * side / 3.0) as i64;
            }
            c.square(sx, sy, side, angle, 1);
        }
    }
    Ok(format!("{}\nGeorg Nees - schotter, plotter on paper, 1968. Redis ver. {REDIS_VERSION}\n", c.braille()))
}

struct Skyscraper {
    xoff: i64,
    width: i64,
    height: i64,
    windows: bool,
    color: u8,
}

fn skyscraper(c: &mut Canvas, s: &Skyscraper, rng: &mut Rng) {
    let start = c.height - 1;
    let end = start - s.height + 1;
    let mut y = start;
    while y >= end {
        for x in s.xoff..s.xoff + s.width {
            // The roof is four pixels narrower.
            if y == end && (x <= s.xoff + 1 || x >= s.xoff + s.width - 2) {
                continue;
            }
            let mut color = s.color;
            if s.windows && x > s.xoff + 1 && x < s.xoff + s.width - 2 && y > end + 1 && y < start - 1 {
                let relx = x - (s.xoff + 1);
                let rely = y - (end + 1);
                // Windows two pixels wide, one tall.
                if (relx / 2) % 2 != 0 && rely % 2 != 0 {
                    loop {
                        color = 1 + (rand(rng) % 2) as u8;
                        if color != s.color {
                            break;
                        }
                    }
                    if relx % 2 != 0 {
                        color = c.get(x - 1, y);
                    }
                }
            }
            c.set(x, y, color);
        }
        y -= 1;
    }
}

/// LOLWUT VERSION 6 [columns] [rows]: a skyline of the 8 bit games' parallax backgrounds.
fn skyline_command(rest: &[Bytes], rng: &mut Rng) -> Result<String, Error> {
    let [cols, rows] = opts(rest, [80, 20], [(1, 1000), (1, 1000)])?;
    let mut c = Canvas::new(cols, rows, 3);
    // Background buildings without windows, the lighter ones behind.
    for color in [2u8, 1] {
        let mut offset = -10;
        while offset < c.width {
            offset += rand(rng) % 8;
            let width = 10 + rand(rng) % 9;
            let height = if color == 2 { c.height / 2 + rand(rng) % c.height / 2 } else { c.height / 2 + rand(rng) % c.height / 3 };
            skyscraper(&mut c, &Skyscraper { xoff: offset, width, height, windows: false, color }, rng);
            offset += if color == 2 { width / 2 } else { width + 1 };
        }
    }
    // Foreground buildings with windows.
    let mut offset = -10;
    while offset < c.width {
        offset += rand(rng) % 8;
        let mut width = 5 + rand(rng) % 14;
        if width % 4 != 0 {
            width += width % 3;
        }
        let height = c.height / 3 + rand(rng) % c.height / 2;
        skyscraper(&mut c, &Skyscraper { xoff: offset, width, height, windows: true, color: 0 }, rng);
        offset += width + 5;
    }
    let mut s = String::new();
    for y in 0..c.height {
        for x in 0..c.width {
            let esc = match c.get(x, y) {
                0 => "0;30;40m",
                1 => "0;90;100m",
                2 => "0;37;47m",
                3 => "0;97;107m",
                _ => "0;30;40m",
            };
            s.push_str("\x1b[");
            s.push_str(esc);
            s.push_str(" \x1b[0m");
        }
        if y != c.height - 1 {
            s.push('\n');
        }
    }
    s.push_str("\nDedicated to the 8 bit game developers of past and present.\nOriginal 8 bit image from Plaguemon by hikikomori. Redis ver. ");
    s.push_str(REDIS_VERSION);
    s.push('\n');
    Ok(s)
}

/// A verse of TAPE MARK I: English and Italian text, its two metrical fractions and its source.
struct Verse {
    en: &'static str,
    it: &'static str,
    f1: &'static [u8; 3],
    f2: &'static [u8; 3],
    group: u8,
}

const VERSES: [Verse; 15] = [
    Verse { en: " The blinding / globe / of fire ", it: " l accecante   /  globo  /  di fuoco  ", f1: b"1/4", f2: b"2/3", group: 1 },
    Verse { en: " It expands / rapidly ", it: " si espande   /  rapidamente  ", f1: b"1/2", f2: b"3/4", group: 1 },
    Verse { en: " Thirty times / brighter / than the sun ", it: " trenta volte  / piu luminoso  / del sole ", f1: b"2/3", f2: b"2/4", group: 1 },
    Verse { en: " When it reaches / the stratosphere ", it: " quando  raggiunge / la stratosfera  ", f1: b"3/4", f2: b"1/2", group: 1 },
    Verse { en: " The summit / of the cloud ", it: " la  sommita  /  della nuvola ", f1: b"1/3", f2: b"2/3", group: 1 },
    Verse { en: " Assumes / the well-known shape / of a mushroom ", it: " assume   / la ben nota forma  / di fungo ", f1: b"2/4", f2: b"3/4", group: 1 },
    Verse { en: " The head / pressed / upon the shoulder ", it: " la testa / premuta  / sulla spalla  ", f1: b"1/4", f2: b"2/4", group: 2 },
    Verse { en: " The hair / between the lips ", it: " i  capelli   /  tra le labbra ", f1: b"1/4", f2: b"2/4", group: 2 },
    Verse { en: " They lay / motionless / without speaking ", it: " giacquero  /   immobili / senza parlare ", f1: b"2/3", f2: b"2/3", group: 2 },
    Verse { en: " Till he moved / his fingers / slowly ", it: " finche non mosse  /  le dita  / lentamente    ", f1: b"3/4", f2: b"1/3", group: 2 },
    Verse { en: " Trying / to grasp ", it: " cercando / di afferrare  ", f1: b"3/4", f2: b"1/2", group: 2 },
    Verse {
        en: " While the multitude / of things / comes into being ",
        it: " mentre la moltitudine  /  delle cose  /   accade   ",
        f1: b"1/2",
        f2: b"1/2",
        group: 3,
    },
    Verse { en: " I envisage / their return ", it: " io contemplo  /  il loro ritorno    ", f1: b"2/3", f2: b"3/4", group: 3 },
    Verse { en: " Although / things / flourish ", it: " malgrado / che le cose  /  fioriscano    ", f1: b"1/2", f2: b"2/3", group: 3 },
    Verse { en: " They all return / to / their roots ", it: " esse tornano  / tutte    / alla loro radice   ", f1: b"2/3", f2: b"1/4", group: 3 },
];

/// LOLWUT (VERSION 8) [IT]: Nanni Balestrini's TAPE MARK I.
fn tape_mark(rest: &[Bytes], rng: &mut Rng) -> String {
    let english = !rest.first().is_some_and(|a| eq(a, "it"));
    let mut pool: Vec<&Verse> = VERSES.iter().collect();
    // Fisher-Yates.
    for i in (1..pool.len()).rev() {
        let j = (rand(rng) % (i as i64 + 1)) as usize;
        pool.swap(i, j);
    }
    let mut stanza: Vec<&Verse> = Vec::new();
    while stanza.len() < 10 {
        let fits = |v: &Verse| match stanza.last() {
            None => true,
            Some(p) => (v.f1[0] == p.f2[0] || v.f1[2] == p.f2[0] || v.f1[2] == p.f2[2]) && v.group != p.group,
        };
        let Some(i) = pool.iter().position(|v| fits(v)) else { break };
        stanza.push(pool.remove(i));
    }
    let mut s = String::new();
    for v in &stanza {
        s.push_str(if english { v.en } else { v.it });
        s.push('\n');
    }
    let mut s: String = s.chars().map(|c| if c == '/' { ' ' } else { c.to_ascii_uppercase() }).collect();
    s.push_str(
        "\nIn 1961, Nanni Balestrini created one of the first computer-generated poems, TAPE MARK I, using an IBM 7090 mainframe. Each execution combined verses from three literary sources following algorithmic rules based on metrical compatibility and group constraints. This LOLWUT command reproduces Balestrini's original algorithm, generating new stanzas through the same computational poetry process described in Almanacco Letterario Bompiani, 1962.\n\n\
         https://en.wikipedia.org/wiki/Digital_poetry\n\
         https://www.youtube.com/watch?v=8i7uFCK7G0o (English subs)\n\n\
         Use: LOLWUT IT for the original Italian output. Redis ver. ",
    );
    s.push_str(REDIS_VERSION);
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schotter_has_the_grid_size() {
        let mut rng = Rng::seeded(1);
        let s = schotter_command(&[], &mut rng).unwrap();
        let art: Vec<&str> = s.lines().take_while(|l| !l.is_empty()).collect();
        // 66 columns of braille; (side * 12 + 4) / 4 rows, side = (132 - 4) / 8.
        assert!(art.iter().all(|l| l.chars().count() == 66));
        assert_eq!(art.len(), (16 * 12 + 4_usize).div_ceil(4));
    }

    #[test]
    fn tape_mark_alternates_sources() {
        let mut rng = Rng::seeded(7);
        let s = tape_mark(&[], &mut rng);
        assert!(s.contains("Use: LOLWUT IT for the original Italian output. Redis ver. "));
        assert!(!s.split("\n\n").next().unwrap().contains('/'));
    }
}
