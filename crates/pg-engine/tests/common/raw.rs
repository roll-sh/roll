//! A trust-authenticated connection speaking the wire protocol by hand, for what driver
//! libraries don't send (cursors as portals, Describe of a portal, odd counts). `read`
//! returns the messages up to ReadyForQuery as short strings: "T a:23/-1", "D 1|x",
//! "C SELECT 1", "E 42P18", "s" (PortalSuspended).

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Raw {
    s: TcpStream,
    /// The last ReadyForQuery's transaction status.
    pub status: u8,
}

impl Raw {
    pub fn connect(addr: &str, user: &str) -> Raw {
        let s = TcpStream::connect(addr).unwrap();
        let mut body = 196608i32.to_be_bytes().to_vec();
        body.extend_from_slice(format!("user\0{user}\0database\0postgres\0\0").as_bytes());
        let mut m = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        m.extend_from_slice(&body);
        let mut r = Raw { s, status: b'I' };
        r.s.write_all(&m).unwrap();
        r.read();
        r
    }

    pub fn send(&mut self, t: u8, body: &[u8]) {
        let mut m = vec![t];
        m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        m.extend_from_slice(body);
        self.s.write_all(&m).unwrap();
    }

    pub fn query(&mut self, sql: &str) -> Vec<String> {
        self.send(b'Q', format!("{sql}\0").as_bytes());
        self.read()
    }

    pub fn parse(&mut self, name: &str, sql: &str) {
        self.send(b'P', format!("{name}\0{sql}\0\0\0").as_bytes());
    }

    /// Bind with no parameters and text results.
    pub fn bind(&mut self, portal: &str, stmt: &str) {
        self.send(b'B', format!("{portal}\0{stmt}\0\0\0\0\0\0\0").as_bytes());
    }

    pub fn describe(&mut self, kind: u8, name: &str) {
        let mut b = vec![kind];
        b.extend_from_slice(format!("{name}\0").as_bytes());
        self.send(b'D', &b);
    }

    pub fn execute(&mut self, portal: &str, max_rows: i32) {
        let mut b = format!("{portal}\0").into_bytes();
        b.extend_from_slice(&max_rows.to_be_bytes());
        self.send(b'E', &b);
    }

    pub fn close(&mut self, kind: u8, name: &str) {
        let mut b = vec![kind];
        b.extend_from_slice(format!("{name}\0").as_bytes());
        self.send(b'C', &b);
    }

    pub fn sync(&mut self) -> Vec<String> {
        self.send(b'S', b"");
        self.read()
    }

    fn message(&mut self) -> (u8, Vec<u8>) {
        let mut h = [0u8; 5];
        self.s.read_exact(&mut h).unwrap();
        let n = i32::from_be_bytes(h[1..].try_into().unwrap()) as usize - 4;
        let mut b = vec![0u8; n];
        self.s.read_exact(&mut b).unwrap();
        (h[0], b)
    }

    /// The next message, if one comes within `wait` (its type and body).
    pub fn next_within(&mut self, wait: std::time::Duration) -> Option<(u8, Vec<u8>)> {
        self.s.set_read_timeout(Some(wait)).unwrap();
        let mut t = [0u8; 1];
        let got = self.s.read(&mut t);
        self.s.set_read_timeout(None).unwrap();
        match got {
            Ok(1) => {}
            _ => return None,
        }
        let mut n = [0u8; 4];
        self.s.read_exact(&mut n).unwrap();
        let mut b = vec![0u8; i32::from_be_bytes(n) as usize - 4];
        self.s.read_exact(&mut b).unwrap();
        Some((t[0], b))
    }

    pub fn read(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let (t, b) = self.message();
            match t {
                b'Z' => {
                    self.status = b[0];
                    return out;
                }
                b'E' => {
                    let code = b
                        .split(|c| *c == 0)
                        .find(|f| f.first() == Some(&b'C'))
                        .map(|f| String::from_utf8_lossy(&f[1..]).into_owned())
                        .unwrap_or_default();
                    out.push(format!("E {code}"));
                }
                b'T' => out.push(format!("T {}", row_description(&b).join(" "))),
                b'D' => out.push(format!("D {}", data_row(&b).join("|"))),
                b'C' => out.push(format!("C {}", String::from_utf8_lossy(&b[..b.len() - 1]))),
                b's' => out.push("s".into()),
                b'n' => out.push("n".into()),
                b'N' | b'S' | b'1' | b'2' | b'3' | b't' | b'K' | b'R' => {}
                t => out.push((t as char).to_string()),
            }
        }
    }
}

/// "name:typeoid/typmod" per column.
fn row_description(b: &[u8]) -> Vec<String> {
    let (mut cols, mut at) = (Vec::new(), 2);
    for _ in 0..u16::from_be_bytes([b[0], b[1]]) {
        let e = at + b[at..].iter().position(|c| *c == 0).unwrap();
        let oid = u32::from_be_bytes(b[e + 7..e + 11].try_into().unwrap());
        let typmod = i32::from_be_bytes(b[e + 13..e + 17].try_into().unwrap());
        cols.push(format!("{}:{oid}/{typmod}", String::from_utf8_lossy(&b[at..e])));
        at = e + 19;
    }
    cols
}

fn data_row(b: &[u8]) -> Vec<String> {
    let (mut vals, mut at) = (Vec::new(), 2);
    for _ in 0..u16::from_be_bytes([b[0], b[1]]) {
        let n = i32::from_be_bytes(b[at..at + 4].try_into().unwrap());
        at += 4;
        if n < 0 {
            vals.push("NULL".to_string());
        } else {
            vals.push(String::from_utf8_lossy(&b[at..at + n as usize]).into_owned());
            at += n as usize;
        }
    }
    vals
}
