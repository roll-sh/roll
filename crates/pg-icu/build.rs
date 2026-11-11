//! Builds ICU4C 78.1 (the reference server's ICU) from its release sources, once, into a
//! cache under the target directory (`<target>/icu4c/78.1-<triple>/`): the static
//! libraries of ICU's common and i18n parts, and the data package the crate embeds.
//!
//! - The source tarball is downloaded (curl or wget) and checked against the release's
//!   SHA-512, or taken from `PG_ICU_SOURCE` (offline builds).
//! - configure && make -j$NUM_JOBS in common, i18n and stubdata only (no tools, tests,
//!   samples or data build): about a minute on two cores. The cache keeps the tarball,
//!   the static libraries and the data (~50 MB).
//! - The data is the release's prebuilt icudt78l.dat without what nothing here uses
//!   (converters, display names, currencies, units, zones, RBNF, transliterators):
//!   collation, break iteration, locale and property data stay, unchanged.
//!
//! Anything going wrong (no network, no C++ compiler, a missing TMPDIR, a cross build) fails
//! the build: without ICU every ICU collation would fail or, under pg-engine's `icu4x`
//! feature, only approximate the reference's, so results would silently differ. Building
//! without ICU (`available()` false) takes `PG_ICU_DISABLE=1`. `PG_ICU_CACHE` moves the cache.

use sha2::{Digest, Sha512};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = "78.1";
const DATA_NAME: &str = "icudt78l";
const URL: &str =
    "https://github.com/unicode-org/icu/releases/download/release-78.1/icu4c-78.1-sources.tgz";
const SHA512: &str = "c366398fdb50afc6355a8c45ed1d68a18eaa5f07a5d1c4555becbcfb9d4073e65ebe1e9caf24b93779b11b36cd813c98dd59e4b19f008851f25c7262811c112d";
const LIBS: [&str; 3] = ["icui18n", "icuuc", "icudata"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for v in ["PG_ICU_DISABLE", "PG_ICU_CACHE", "PG_ICU_SOURCE"] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    println!("cargo::rustc-check-cfg=cfg(pg_icu)");
    if env("PG_ICU_DISABLE").is_some_and(|v| v != "0") {
        println!("cargo:warning=pg-icu: PG_ICU_DISABLE is set, building without ICU4C {VERSION}");
        return;
    }
    match build() {
        Ok(dir) => {
            println!("cargo:rerun-if-changed={}", dir.join("ok").display());
            println!("cargo:rustc-link-search=native={}", dir.join("lib").display());
            for l in LIBS {
                println!("cargo:rustc-link-lib=static={l}");
            }
            let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
            println!(
                "cargo:rustc-link-lib={}",
                if os == "macos" { "c++" } else { "stdc++" }
            );
            println!(
                "cargo:rustc-env=PG_ICU_DATA={}",
                dir.join(format!("{DATA_NAME}.dat")).display()
            );
            println!("cargo:rustc-cfg=pg_icu");
        }
        // (A failed build script runs again on the next build, which retries.)
        Err(e) => panic!(
            "pg-icu: could not build ICU4C {VERSION}: {e}\n\
             ICU collations would be wrong or unavailable without it. Fix the cause and \
             rebuild, or set PG_ICU_DISABLE=1 to build without ICU on purpose."
        ),
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// The cache: `PG_ICU_CACHE`, or `icu4c` in the target directory (the nearest ancestor of
/// OUT_DIR with cargo's CACHEDIR.TAG).
fn cache_dir() -> Result<PathBuf, String> {
    if let Some(d) = env("PG_ICU_CACHE") {
        return Ok(PathBuf::from(d));
    }
    let out = PathBuf::from(env("OUT_DIR").ok_or("OUT_DIR not set")?);
    let target = out
        .ancestors()
        .find(|a| a.join("CACHEDIR.TAG").is_file())
        .unwrap_or(&out);
    Ok(target.join("icu4c"))
}

fn build() -> Result<PathBuf, String> {
    let target = env("TARGET").unwrap_or_default();
    if env("HOST").as_deref() != Some(target.as_str()) {
        return Err(format!("cross build to {target}"));
    }
    if env("CARGO_CFG_TARGET_ENDIAN").as_deref() != Some("little") {
        return Err("big-endian target".into());
    }
    if !matches!(
        env("CARGO_CFG_TARGET_OS").as_deref(),
        Some("macos" | "linux")
    ) {
        return Err(format!("unsupported target {target}"));
    }
    let cache = cache_dir()?;
    let done = cache.join(format!("{VERSION}-{target}"));
    if done.join("ok").is_file() {
        return Ok(done);
    }
    fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    // One builder at a time (concurrent cargo runs on other target directories).
    let lock = fs::File::create(cache.join(".lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    if done.join("ok").is_file() {
        return Ok(done);
    }
    let src = source(&cache)?;
    let build = cache.join(format!("build-{VERSION}-{target}"));
    let libdir = compile(&src, &build)?;
    let tmp = cache.join(format!("{VERSION}-{target}.tmp"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("lib")).map_err(|e| e.to_string())?;
    for l in LIBS {
        let f = format!("lib{l}.a");
        // (The stub data library stays in its directory.)
        let from = match l {
            "icudata" => build.join("stubdata").join(&f),
            _ => libdir.join(&f),
        };
        fs::copy(from, tmp.join("lib").join(&f)).map_err(|e| format!("{f}: {e}"))?;
    }
    let dat = fs::read(src.join(format!("data/in/{DATA_NAME}.dat")))
        .map_err(|e| format!("{DATA_NAME}.dat: {e}"))?;
    let dat = filter_data(&dat)?;
    fs::write(tmp.join(format!("{DATA_NAME}.dat")), dat).map_err(|e| e.to_string())?;
    fs::write(tmp.join("ok"), VERSION).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&done);
    fs::rename(&tmp, &done).map_err(|e| e.to_string())?;
    // The objects and the unpacked sources aren't needed any more (the tarball stays).
    let _ = fs::remove_dir_all(&build);
    let _ = fs::remove_dir_all(cache.join(format!("src-{VERSION}")));
    Ok(done)
}

/// The unpacked `icu/source` directory, downloading and checking the tarball if needed.
fn source(cache: &Path) -> Result<PathBuf, String> {
    let src = cache.join(format!("src-{VERSION}"));
    let root = src.join("icu/source");
    if root.join("configure").is_file() && root.join("data/in").is_dir() {
        return Ok(root);
    }
    let tarball = match env("PG_ICU_SOURCE") {
        Some(p) => PathBuf::from(p),
        None => {
            let t = cache.join(format!("icu4c-{VERSION}-sources.tgz"));
            if !t.is_file() {
                download(&t)?;
            }
            t
        }
    };
    let bytes = fs::read(&tarball).map_err(|e| format!("{}: {e}", tarball.display()))?;
    let sum: String = Sha512::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if sum != SHA512 {
        let _ = fs::remove_file(&tarball);
        return Err(format!("{} has the wrong SHA-512", tarball.display()));
    }
    let tmp = cache.join(format!("src-{VERSION}.tmp"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    run(
        Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&tmp),
        None,
    )?;
    let _ = fs::remove_dir_all(&src);
    fs::rename(&tmp, &src).map_err(|e| e.to_string())?;
    Ok(root)
}

fn download(to: &Path) -> Result<(), String> {
    let part = to.with_extension("part");
    let ok = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&part)
        .arg(URL)
        .status()
        .is_ok_and(|s| s.success())
        || Command::new("wget")
            .args(["-q", "-O"])
            .arg(&part)
            .arg(URL)
            .status()
            .is_ok_and(|s| s.success());
    if !ok {
        let _ = fs::remove_file(&part);
        return Err(format!("could not download {URL}"));
    }
    fs::rename(&part, to).map_err(|e| e.to_string())
}

/// configure and make the static libraries; their directory.
fn compile(src: &Path, build: &Path) -> Result<PathBuf, String> {
    // (configure's temporary files go there; a missing one fails it obscurely.)
    if let Some(t) = env("TMPDIR").filter(|t| !Path::new(t).is_dir()) {
        return Err(format!("TMPDIR {t} is not a directory"));
    }
    fs::create_dir_all(build).map_err(|e| e.to_string())?;
    let log = build.join("build.log");
    if !build.join("config.status").is_file() {
        let mut c = Command::new("sh");
        c.arg(src.join("configure"))
            .args([
                "--enable-static",
                "--disable-shared",
                "--disable-tests",
                "--disable-samples",
                "--disable-extras",
                "--disable-icuio",
                "--disable-layoutex",
                "--disable-tools",
                "--disable-dyload",
                "--with-data-packaging=archive",
            ])
            .env("CFLAGS", "-O2 -fPIC")
            .env("CXXFLAGS", "-O2 -fPIC")
            .current_dir(build);
        run(&mut c, Some(&log))?;
    }
    for d in ["lib", "bin"] {
        fs::create_dir_all(build.join(d)).map_err(|e| e.to_string())?;
    }
    let jobs = env("NUM_JOBS")
        .and_then(|j| j.parse::<usize>().ok())
        .unwrap_or(2)
        .max(1);
    for dir in ["stubdata", "common", "i18n"] {
        let mut c = Command::new("make");
        c.arg(format!("-j{jobs}"))
            .arg("-C")
            .arg(dir)
            .current_dir(build)
            // Not cargo's jobserver: the -j above is the budget.
            .env_remove("MAKEFLAGS")
            .env_remove("MFLAGS")
            .env_remove("CARGO_MAKEFLAGS");
        run(&mut c, Some(&log))?;
    }
    Ok(build.join("lib"))
}

fn run(c: &mut Command, log: Option<&Path>) -> Result<(), String> {
    if let Some(log) = log {
        let f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| e.to_string())?;
        let f2 = f.try_clone().map_err(|e| e.to_string())?;
        c.stdout(f).stderr(f2);
    }
    let st = c.status().map_err(|e| format!("{c:?}: {e}"))?;
    if st.success() {
        return Ok(());
    }
    let tail = log
        .and_then(|l| fs::read_to_string(l).ok())
        .map(|s| {
            let lines: Vec<&str> = s.lines().collect();
            lines[lines.len().saturating_sub(5)..].join(" | ")
        })
        .unwrap_or_default();
    Err(format!("{c:?} failed ({st}): {tail}"))
}

/// The common data package (`icupkg` format: header, table of contents, 16-byte aligned
/// items) without the items nothing here loads.
fn filter_data(d: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "malformed ICU data package".to_string();
    let u32_at = |i: usize| -> Result<usize, String> {
        Ok(u32::from_le_bytes(d.get(i..i + 4).ok_or_else(bad)?.try_into().map_err(|_| bad())?) as usize)
    };
    let hs = u16::from_le_bytes([d[0], d[1]]) as usize;
    if d.len() < hs + 4 || d[2] != 0xda || d[3] != 0x27 || !hs.is_multiple_of(16) {
        return Err(bad());
    }
    let n = u32_at(hs)?;
    let mut items = Vec::with_capacity(n);
    for i in 0..n {
        let name_at = hs + u32_at(hs + 4 + 8 * i)?;
        let data_at = hs + u32_at(hs + 8 + 8 * i)?;
        let end = d[name_at..].iter().position(|&b| b == 0).ok_or_else(bad)?;
        items.push((&d[name_at..name_at + end], data_at));
    }
    let mut kept: Vec<(&[u8], &[u8])> = Vec::new();
    for (i, &(name, at)) in items.iter().enumerate() {
        let end = items.get(i + 1).map_or(d.len(), |x| x.1);
        let short = std::str::from_utf8(name).map_err(|_| bad())?;
        let short = short.split_once('/').map_or(short, |x| x.1);
        let keep = match short.split_once('/') {
            Some((dir, _)) => matches!(dir, "coll" | "brkitr"),
            None => !short.ends_with(".cnv")
                && !short.ends_with(".spp")
                && !short.ends_with(".cfu"),
        };
        if keep {
            kept.push((name, d.get(at..end).ok_or_else(bad)?));
        }
    }
    let mut out = d[..hs].to_vec();
    let names_at = 4 + 8 * kept.len();
    let names_len: usize = kept.iter().map(|k| k.0.len() + 1).sum();
    let mut data_at = (names_at + names_len).next_multiple_of(16);
    out.extend((kept.len() as u32).to_le_bytes());
    let mut name_at = names_at;
    let mut layout = Vec::with_capacity(kept.len());
    for &(name, data) in &kept {
        out.extend((name_at as u32).to_le_bytes());
        out.extend((data_at as u32).to_le_bytes());
        layout.push(data_at);
        name_at += name.len() + 1;
        data_at = (data_at + data.len()).next_multiple_of(16);
    }
    for &(name, _) in &kept {
        out.extend(name);
        out.push(0);
    }
    for (&(_, data), &at) in kept.iter().zip(&layout) {
        out.resize(hs + at, 0xaa);
        out.extend(data);
    }
    Ok(out)
}
