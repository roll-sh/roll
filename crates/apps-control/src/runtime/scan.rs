//! Image vulnerability scanning on one node's Docker (Cloud Run's Artifact Analysis; docs/apps/
//! README.md, "Vulnerability scanning"): Trivy, pinned by digest, reads the image as a saved
//! archive with no network, as rolld's own user, read-only but for its report, and the
//! vulnerability database is a volume only the download step writes (at most twice a day; an older
//! one goes on when a download fails). The image's own files are the input of a parser, never
//! code that runs.

use crate::model::{Severity, Vulnerability};
use anyhow::{Context, bail};
use serde_json::Value;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

/// The only scanner roll runs, by digest.
pub const TRIVY: &str = "aquasec/trivy:0.76.1@sha256:790fd66d1697395d8693d1acaa1ea702406a20f109b6fc7bac8db623eca6c06c";
pub const SCANNER: &str = "trivy 0.76.1";
/// The vulnerability database's volume (written only by the download).
const DB_VOLUME: &str = "roll-apps-trivy-db";
/// How old the database may get before a scan downloads it again.
const DB_FRESH: Duration = Duration::from_secs(12 * 3600);
/// The most of a report read: a bigger one is an image with more findings than anyone reads.
const MAX_REPORT: u64 = 64 << 20;
/// The most of an SBOM kept (CycloneDX JSON); a bigger one isn't kept, its scan still is.
const MAX_SBOM: u64 = 8 << 20;

/// The scanner's state on this node: until when its database is fresh, and one scan at a time
/// (each takes a CPU or two and up to 2 GiB for a few seconds).
pub struct Scanner {
    db_until: tokio::sync::Mutex<Option<Instant>>,
    one: tokio::sync::Semaphore,
}

impl Default for Scanner {
    fn default() -> Scanner {
        Scanner { db_until: Default::default(), one: tokio::sync::Semaphore::new(1) }
    }
}

impl Scanner {

    /// An image's known vulnerabilities, most severe first, and its SBOM (CycloneDX JSON) when
    /// it's small enough to keep. `service` labels what it runs.
    pub async fn scan(&self, image: &str, service: &str, note: impl Fn(String)) -> anyhow::Result<(Vec<Vulnerability>, Option<Vec<u8>>)> {
        let _one = self.one.acquire().await?;
        self.fresh_db(&note).await?;
        let dir = tempfile::Builder::new().prefix("roll-apps-scan-").tempdir().context("a directory for the scan")?;
        let tar = dir.path().join("image.tar");
        let out = dir.path().join("out");
        std::fs::create_dir(&out)?;
        super::docker::docker_within(&["save", "-o", &tar.to_string_lossy(), image], Some(Duration::from_secs(600))).await.context("saving the image to scan it")?;
        let meta = std::fs::metadata(dir.path())?;
        let user = format!("{}:{}", meta.uid(), meta.gid());
        let name = format!("roll-apps-scan-{}", crate::model::random(10).to_ascii_lowercase());
        let label = format!("sh.roll.apps={service}");
        let (scan_mount, out_mount) = (format!("{}:/scan:ro", dir.path().display()), format!("{}:/out", out.display()));
        let db = format!("{DB_VOLUME}:/db:ro");
        // The report lists every package, so the same run's CycloneDX SBOM is its conversion.
        let run = "trivy image --cache-dir /db --input /scan/image.tar --skip-db-update --skip-java-db-update --offline-scan --cache-backend memory --scanners vuln --list-all-pkgs --format json --output /out/report.json --timeout 8m --quiet && trivy convert --format cyclonedx --output /out/sbom.json --quiet /out/report.json";
        let args = [
            "run", "--rm", "--name", &name, "--label", &label, "--user", &user,
            "--network", "none", "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
            "--memory", "2g", "--cpus", "2", "--pids-limit", "256", "--tmpfs", "/tmp:size=256m",
            "-v", &db, "-v", &scan_mount, "-v", &out_mount, "--entrypoint", "sh", TRIVY, "-c", run,
        ];
        if let Err(e) = super::docker::docker_within(&args, Some(Duration::from_secs(600))).await {
            // A CLI that gave up leaves its container running.
            let _ = super::docker::docker(&["rm", "-f", &name]).await;
            return Err(e.context("the scan failed"));
        }
        let report = out.join("report.json");
        let size = std::fs::symlink_metadata(&report).context("the scan wrote no report")?;
        if !size.is_file() || size.len() > MAX_REPORT {
            bail!("the scan's report is over {} MiB: more findings than are read", MAX_REPORT >> 20);
        }
        let found = parse_trivy(&std::fs::read(&report)?)?;
        let sbom = std::fs::symlink_metadata(out.join("sbom.json")).ok().filter(|m| m.is_file() && m.len() <= MAX_SBOM).and_then(|_| std::fs::read(out.join("sbom.json")).ok());
        Ok((found, sbom))
    }

    /// The database, downloaded when it's older than [`DB_FRESH`] (the scan's only network).
    async fn fresh_db(&self, note: &impl Fn(String)) -> anyhow::Result<()> {
        let mut until = self.db_until.lock().await;
        if until.is_some_and(|t| Instant::now() < t) {
            return Ok(());
        }
        let name = format!("roll-apps-trivy-db-{}", crate::model::random(10).to_ascii_lowercase());
        let db = format!("{DB_VOLUME}:/db");
        let args = [
            "run", "--rm", "--name", &name, "--label", "sh.roll.apps=scanner", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--memory", "2g", "--tmpfs", "/tmp:size=1g", "-v", &db, TRIVY,
            "image", "--cache-dir", "/db", "--download-db-only", "--quiet",
        ];
        match super::docker::docker_within(&args, Some(Duration::from_secs(300))).await {
            Ok(_) => {
                *until = Some(Instant::now() + DB_FRESH);
                Ok(())
            }
            Err(e) => {
                let _ = super::docker::docker(&["rm", "-f", &name]).await;
                // One downloaded before (by this process or an earlier one) goes on, tried again
                // in ten minutes.
                if super::docker::docker(&["volume", "inspect", DB_VOLUME]).await.is_ok() {
                    note(format!("the vulnerability database couldn't be updated ({e:#}); scanning with the one from before"));
                    *until = Some(Instant::now() + Duration::from_secs(600));
                    Ok(())
                } else {
                    Err(e.context("downloading the vulnerability database"))
                }
            }
        }
    }
}

/// Trivy's JSON report as findings: one per advisory, package, version and place, most severe
/// first (then those with a fix), at most [`crate::model::MAX_VULNERABILITIES`] kept by the caller.
pub fn parse_trivy(report: &[u8]) -> anyhow::Result<Vec<Vulnerability>> {
    let v: Value = serde_json::from_slice(report).context("the scan's report isn't JSON")?;
    let text = |x: &Value, k: &str, max: usize| x.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| s.chars().take(max).collect::<String>());
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in v.get("Results").and_then(Value::as_array).into_iter().flatten() {
        let target = text(r, "Target", 200).unwrap_or_default();
        // The archive's path inside the scanner is ours, not the image's.
        let target = target.strip_prefix("/scan/image.tar").map(|t| t.trim().trim_start_matches('(').trim_end_matches(')').to_string()).unwrap_or(target);
        for f in r.get("Vulnerabilities").and_then(Value::as_array).into_iter().flatten() {
            let (Some(id), Some(package)) = (text(f, "VulnerabilityID", 80), text(f, "PkgName", 200)) else { continue };
            let installed = text(f, "InstalledVersion", 100).unwrap_or_default();
            if !seen.insert((id.clone(), package.clone(), installed.clone(), target.clone())) {
                continue;
            }
            out.push(Vulnerability {
                severity: Severity::parse(f.get("Severity").and_then(Value::as_str).unwrap_or("")),
                fixed: text(f, "FixedVersion", 200),
                title: text(f, "Title", 200),
                id,
                package,
                installed,
                target: target.clone(),
            });
        }
    }
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.fixed.is_some().cmp(&a.fixed.is_some())).then(a.id.cmp(&b.id)).then(a.package.cmp(&b.package)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trivy_report_reads_as_findings_most_severe_first() {
        let report = br#"{"Results": [
            {"Target": "/scan/image.tar (debian 12.15)", "Vulnerabilities": [
                {"VulnerabilityID": "CVE-2011-3374", "PkgName": "apt", "InstalledVersion": "2.6.2", "Severity": "LOW"},
                {"VulnerabilityID": "CVE-2025-0001", "PkgName": "openssl", "InstalledVersion": "3.0.1", "FixedVersion": "3.0.2", "Severity": "CRITICAL", "Title": "a title"},
                {"VulnerabilityID": "CVE-2025-0001", "PkgName": "openssl", "InstalledVersion": "3.0.1", "FixedVersion": "3.0.2", "Severity": "CRITICAL"}
            ]},
            {"Target": "Node.js", "Vulnerabilities": [{"VulnerabilityID": "GHSA-x", "PkgName": "cross-spawn", "InstalledVersion": "7.0.3", "Severity": "HIGH"}]},
            {"Target": "app/package-lock.json"}
        ]}"#;
        let found = parse_trivy(report).unwrap();
        let ids: Vec<(&str, Severity, &str)> = found.iter().map(|v| (v.id.as_str(), v.severity, v.target.as_str())).collect();
        assert_eq!(ids, [("CVE-2025-0001", Severity::Critical, "debian 12.15"), ("GHSA-x", Severity::High, "Node.js"), ("CVE-2011-3374", Severity::Low, "debian 12.15")], "one per advisory, package and place");
        assert_eq!(found[0].fixed.as_deref(), Some("3.0.2"));
        let s = crate::model::ScanSummary::of(&found, SCANNER, 1);
        assert_eq!((s.critical, s.high, s.low, s.fixable, s.at_least(Severity::High)), (1, 1, 1, 1, 2));
        assert!(parse_trivy(b"not json").is_err());
    }
}
