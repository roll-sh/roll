//! Challenge mode (Vercel's Attack Challenge Mode; docs/apps/README.md, "Firewall rules"): a
//! firewall rule's `challenge` action answers a browser that hasn't passed with a small page that
//! works out a proof of work (a SHA-256 with `BITS` leading zero bits over a seed only roll can
//! make), and posts it to `/__roll/challenge` on the environment's own host. A right answer gets a
//! pass cookie (`roll_pass_<env>`, an hour) bound to the client's network (its /24, or /64), and
//! the page goes on to what it asked for. A script that runs no JavaScript stays out; one that
//! does pays the work for each network an hour. Seeds and passes are HMACs under the
//! environment's own key (kept in the store, made the first time a rule needs it), so any node
//! checks them.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Where a page posts its answer.
pub const PATH: &str = "/__roll/challenge";
/// Leading zero bits a proof needs: about 130,000 hashes, under a second in a browser.
pub const BITS: u32 = 17;
/// How long a seed may be answered (in minutes), and how long a pass lasts.
const SEED_MINUTES: i64 = 3;
pub const PASS_S: i64 = 3600;

/// The pass cookie's name in an environment.
pub fn cookie(env: &str) -> String {
    format!("roll_challenge_{env}")
}

/// The network a client counts as: its /24, or its /64 (one host has the whole of either to
/// rotate through, and a pass shouldn't break when it does).
pub fn network(client: Option<std::net::IpAddr>) -> String {
    match client {
        Some(std::net::IpAddr::V4(v4)) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        Some(std::net::IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        None => "-".into(),
    }
}

pub(crate) fn mac(key: &[u8], parts: &[&str]) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    for p in parts {
        m.update(p.as_bytes());
        m.update(&[0]);
    }
    m.finalize().into_bytes().iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// The seed a page works on: this network's, this minute's.
pub fn seed(key: &[u8], net: &str, now_ms: i64) -> String {
    mac(key, &["seed", net, &(now_ms / 60_000).to_string()])
}

/// Whether `nonce` answers `seed`, a seed made for `net` in the last few minutes.
pub fn answers(key: &[u8], net: &str, now_ms: i64, seed: &str, nonce: &str) -> bool {
    let minute = now_ms / 60_000;
    let fresh = (0..SEED_MINUTES).any(|ago| crate::public::same(&mac(key, &["seed", net, &(minute - ago).to_string()]), seed));
    if !fresh || nonce.is_empty() || nonce.len() > 32 || !nonce.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    leading_zero_bits(&Sha256::digest(format!("{seed}:{nonce}").as_bytes())) >= BITS
}

fn leading_zero_bits(h: &[u8]) -> u32 {
    let mut n = 0;
    for b in h {
        if *b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

/// A pass for `net` until `until` (unix seconds): what the cookie holds.
pub fn pass(key: &[u8], net: &str, until: i64) -> String {
    format!("{until}.{}", mac(key, &["pass", net, &until.to_string()]))
}

/// Whether a cookie's pass is good for `net` now.
pub fn passes(key: &[u8], net: &str, now_s: i64, cookie: &str) -> bool {
    let Some((until, sig)) = cookie.split_once('.') else { return false };
    let Ok(t) = until.parse::<i64>() else { return false };
    t > now_s && t <= now_s + PASS_S && crate::public::same(&mac(key, &["pass", net, until]), sig)
}

/// The page a browser that hasn't passed gets: it works the seed out and posts the answer.
pub fn page(seed: &str, to: &str) -> String {
    let to_js = serde_json::to_string(to).expect("a string serializes").replace('<', "\\u003c");
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><meta name=robots content=noindex><title>Checking your browser</title>\
         <style>:root{{color-scheme:dark light}}body{{margin:0;min-height:100vh;display:grid;place-items:center;font:14px system-ui,sans-serif;background:#000;color:#ededed}}@media (prefers-color-scheme:light){{body{{background:#fff;color:#0a0a0a}}}}p{{opacity:.7}}</style>\
         <main><h1 style=\"font-size:18px;font-weight:600;letter-spacing:-.04em\">Checking your browser</h1><p id=s>A moment…</p><noscript><p>This site needs JavaScript to check that you're a browser.</p></noscript></main>\
         <script>{SHA256}\nconst seed={seed:?},bits={BITS},to={to_js};\
         function z(h){{let n=0;for(const b of h){{if(b===0){{n+=8;continue}}return n+Math.clz32(b)-24}}return n}}\
         (async()=>{{for(let i=0;;i++){{if(z(sha256(seed+':'+i))>=bits){{const r=await fetch('{PATH}',{{method:'POST',headers:{{'content-type':'application/x-www-form-urlencoded'}},body:'seed='+seed+'&nonce='+i}});\
         if(r.ok){{location.replace(to)}}else{{document.getElementById('s').textContent='That took too long. Reload to try again.'}}return}}}}}})();</script>"
    )
}

/// SHA-256 in a few lines of JavaScript (an insecure page has no `crypto.subtle`): bytes of a
/// string's UTF-8.
const SHA256: &str = "function sha256(s){const K=[],H=[];let p=2;for(let n=0;n<64;p++){let q=true;for(let d=2;d*d<=p;d++)if(p%d===0){q=false;break}if(q){if(n<8)H[n]=(Math.pow(p,1/2)%1)*4294967296|0;K[n++]=(Math.pow(p,1/3)%1)*4294967296|0}}\
const m=new TextEncoder().encode(s),l=m.length,w=new Uint32Array(((l+9+63)>>6)<<4);for(let i=0;i<l;i++)w[i>>2]|=m[i]<<(24-(i%4)*8);w[l>>2]|=0x80<<(24-(l%4)*8);w[w.length-1]=l*8;\
const h=H.slice(),W=new Uint32Array(64),r=(x,n)=>(x>>>n)|(x<<(32-n));for(let j=0;j<w.length;j+=16){for(let i=0;i<64;i++){if(i<16)W[i]=w[j+i];else{const a=W[i-15],b=W[i-2];W[i]=(W[i-16]+(r(a,7)^r(a,18)^(a>>>3))+W[i-7]+(r(b,17)^r(b,19)^(b>>>10)))|0}}\
let[a,b,c,d,e,f,g,k]=h;for(let i=0;i<64;i++){const t1=(k+(r(e,6)^r(e,11)^r(e,25))+((e&f)^(~e&g))+K[i]+W[i])|0,t2=((r(a,2)^r(a,13)^r(a,22))+((a&b)^(a&c)^(b&c)))|0;k=g;g=f;f=e;e=(d+t1)|0;d=c;c=b;b=a;a=(t1+t2)|0}\
h[0]=(h[0]+a)|0;h[1]=(h[1]+b)|0;h[2]=(h[2]+c)|0;h[3]=(h[3]+d)|0;h[4]=(h[4]+e)|0;h[5]=(h[5]+f)|0;h[6]=(h[6]+g)|0;h[7]=(h[7]+k)|0}\
const o=new Uint8Array(32);for(let i=0;i<8;i++){o[i*4]=h[i]>>>24;o[i*4+1]=h[i]>>>16&255;o[i*4+2]=h[i]>>>8&255;o[i*4+3]=h[i]&255}return o}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_of_work_earns_a_pass_for_its_network_only() {
        let key = b"k".repeat(32);
        let (net, now) = (network(Some("203.0.113.9".parse().unwrap())), 1_800_000_000_000i64);
        assert_eq!(net, "203.0.113.0/24");
        let s = seed(&key, &net, now);
        let nonce = (0u64..).find(|n| leading_zero_bits(&Sha256::digest(format!("{s}:{n}").as_bytes())) >= BITS).unwrap().to_string();
        assert!(answers(&key, &net, now + 60_000, &s, &nonce), "a minute later still");
        assert!(!answers(&key, &net, now + 5 * 60_000, &s, &nonce), "a stale seed");
        assert!(!answers(&key, "198.51.100.0/24", now, &s, &nonce), "another network's");
        assert!(!answers(&key, &net, now, &s, "1"));
        let p = pass(&key, &net, now / 1000 + PASS_S);
        assert!(passes(&key, &net, now / 1000, &p));
        assert!(!passes(&key, "198.51.100.0/24", now / 1000, &p) && !passes(&key, &net, now / 1000 + PASS_S + 1, &p));
        assert!(!passes(&key, &net, now / 1000, &format!("{}.{}", now / 1000 + PASS_S, "0".repeat(32))));
    }
}
