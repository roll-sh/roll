//! An image built elsewhere (`roll apps deploy --image-file`, Fly's `--local-only`, Heroku's
//! `container:push`): `docker save`'s archive, gzipped or not. Loading an archive applies the
//! names inside it on the host, so one could retag a base image or another service's image
//! there. Before it's loaded its names are taken out: `manifest.json`'s `RepoTags` emptied,
//! `repositories` (the legacy names) left out, and the OCI index's name annotations removed.
//! The image is then tagged `roll-apps-<service>:<deployment>`, as a build's is.

use std::io::{Read, Write};
use std::path::Path;

/// The names an OCI index may carry for an image.
const NAME_ANNOTATIONS: [&str; 2] = ["io.containerd.image.name", "org.opencontainers.image.ref.name"];

/// `input` (a `docker save` archive) written to `out` without any name in it.
pub fn untagged(input: &[u8], out: &Path) -> anyhow::Result<()> {
    let reader: Box<dyn Read> = if input.starts_with(&[0x1f, 0x8b]) { Box::new(flate2::read::GzDecoder::new(input)) } else { Box::new(input) };
    let mut archive = tar::Archive::new(reader);
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(out)?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(file));
    let mut manifest = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.to_string_lossy().trim_start_matches("./").to_string();
        anyhow::ensure!(!path.is_absolute() && !path.components().any(|c| matches!(c, std::path::Component::ParentDir)), "the archive names {name:?}");
        let kind = entry.header().entry_type();
        anyhow::ensure!(kind.is_file() || kind.is_dir(), "{name} isn't a file or a directory");
        match name.as_str() {
            "repositories" => continue,
            "manifest.json" | "index.json" => {
                let mut text = Vec::new();
                entry.take(4 << 20).read_to_end(&mut text)?;
                let mut v: serde_json::Value = serde_json::from_slice(&text).map_err(|e| anyhow::anyhow!("{name} isn't JSON: {e}"))?;
                if name == "manifest.json" {
                    manifest = true;
                    for m in v.as_array_mut().into_iter().flatten() {
                        m["RepoTags"] = serde_json::Value::Null;
                    }
                } else {
                    strip_names(&mut v);
                }
                let body = serde_json::to_vec(&v)?;
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                builder.append_data(&mut h, &name, body.as_slice())?;
            }
            _ => {
                let mut h = entry.header().clone();
                builder.append_data(&mut h, &name, &mut entry)?;
            }
        }
    }
    anyhow::ensure!(manifest, "that isn't an image archive (`docker save`'s, with a manifest.json)");
    builder.into_inner()?.flush()?;
    Ok(())
}

/// An OCI index without the annotations that name its images, at any depth.
fn strip_names(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(o) => {
            if let Some(serde_json::Value::Object(a)) = o.get_mut("annotations") {
                for k in NAME_ANNOTATIONS {
                    a.remove(k);
                }
            }
            for x in o.values_mut() {
                strip_names(x);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip_names),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    fn archive(files: &[(&str, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, body) in files {
            // The name written as it is (a hostile one too: `..`).
            let mut h = tar::Header::new_old();
            h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append(&h, body.as_bytes()).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn an_archives_names_are_taken_out() {
        let input = archive(&[
            ("manifest.json", r#"[{"Config":"blobs/sha256/c","RepoTags":["nginx:latest","roll-apps-other:x"],"Layers":["blobs/sha256/l"]}]"#),
            ("repositories", r#"{"nginx":{"latest":"c"}}"#),
            ("index.json", r#"{"manifests":[{"digest":"sha256:m","annotations":{"io.containerd.image.name":"docker.io/library/nginx:latest","org.opencontainers.image.ref.name":"latest","keep":"x"}}]}"#),
            ("blobs/sha256/l", "layer"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.tar");
        super::untagged(&input, &out).unwrap();
        let mut seen = std::collections::BTreeMap::new();
        for e in tar::Archive::new(std::fs::File::open(&out).unwrap()).entries().unwrap() {
            let mut e = e.unwrap();
            let mut s = String::new();
            let name = e.path().unwrap().to_string_lossy().to_string();
            e.read_to_string(&mut s).unwrap();
            seen.insert(name, s);
        }
        assert!(!seen.contains_key("repositories") && seen["blobs/sha256/l"] == "layer", "{seen:?}");
        assert!(!seen["manifest.json"].contains("nginx") && seen["manifest.json"].contains("blobs/sha256/l"), "{}", seen["manifest.json"]);
        assert!(!seen["index.json"].contains("nginx") && !seen["index.json"].contains("ref.name") && seen["index.json"].contains("keep"), "{}", seen["index.json"]);
        // Not an image archive at all.
        assert!(super::untagged(&archive(&[("hello.txt", "hi")]), &dir.path().join("b.tar")).is_err());
        assert!(super::untagged(&archive(&[("../x", "hi"), ("manifest.json", "[]")]), &dir.path().join("c.tar")).is_err());
    }
}
