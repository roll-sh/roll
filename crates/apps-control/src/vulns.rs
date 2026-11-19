//! Known vulnerabilities in deployments' images (Cloud Run's Artifact Analysis, Render's and
//! Railway's image scans; docs/apps/README.md, "Vulnerability scanning"): each deployment's image
//! is scanned by the builder ([`crate::runtime::Builder::scan`]) once it's built or pulled, beside
//! its rollout, or before it with `run.block_on`; the live ones again every day, as advisories
//! come out. Advisories at high or worse that the one before didn't have are `deploy.vulnerable`.

use crate::logs::{LogSink, Stream};
use crate::model::{MAX_VULNERABILITIES, ScanSummary, Severity, Vulnerability};
use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

/// How often a live deployment is scanned again, and how often they're looked at for it.
const RESCAN_MS: i64 = 24 * 3600 * 1000;
const LOOK_MS: i64 = 10 * 60 * 1000;

impl Apps {
    /// Scan a deployment's image and keep what it found: None when this node's builder doesn't
    /// scan. A scan that fails is kept as its error.
    pub(crate) async fn scan_deployment(self: &Arc<Self>, service: &str, env: &str, dep: &str, image: &str, log: &LogSink) -> Option<ScanSummary> {
        let now = code_sql::now_ms();
        let (summary, mut found) = match self.deps.builder.scan(image, service, log).await {
            Ok(None) => return None,
            Ok(Some(s)) => (ScanSummary::of(&s.found, &s.scanner, now), s.found),
            Err(e) => (ScanSummary { scanned_at: now, scanner: crate::runtime::scan::SCANNER.into(), error: Some(format!("{e:#}").chars().take(300).collect()), ..Default::default() }, Vec::new()),
        };
        found.truncate(MAX_VULNERABILITIES);
        let worst = |v: &&Vulnerability| v.severity >= Severity::High;
        let ids: HashSet<String> = found.iter().filter(worst).map(|v| v.id.clone()).collect();
        let (sid, e, d, s2, f2) = (service.to_string(), env.to_string(), dep.to_string(), summary.clone(), found.clone());
        // What was known before (this deployment's last scan, else the environment's newest
        // earlier deployment's), then this scan, in one write.
        let known = self
            .db(move |s| {
                let mut known: Option<HashSet<String>> = s.scan(&d)?.filter(|(sum, _)| sum.error.is_none()).map(|(_, items)| items.into_iter().filter(|v| v.severity >= Severity::High).map(|v| v.id).collect());
                if known.is_none() {
                    let this = s.deployment(&d)?;
                    for other in s.deployments(&sid, &e, 100, None)?.into_iter().filter(|o| o.created_at < this.created_at) {
                        if let Some((sum, items)) = s.scan(&other.id)?
                            && sum.error.is_none()
                        {
                            known = Some(items.into_iter().filter(|v| v.severity >= Severity::High).map(|v| v.id).collect());
                            break;
                        }
                    }
                }
                s.set_scan(&sid, &d, &s2, &f2)?;
                Ok(known.unwrap_or_default())
            })
            .await;
        let known = match known {
            Ok(k) => k,
            Err(err) => {
                tracing::warn!(service, deployment = dep, error = %err, "apps: a scan's findings weren't kept");
                return Some(summary);
            }
        };
        match &summary.error {
            Some(why) => log.line(format!("the image's scan for known vulnerabilities didn't finish: {why}")),
            None => log.line(format!(
                "known vulnerabilities: {} critical, {} high, {} medium, {} low ({} with a fix)",
                summary.critical, summary.high, summary.medium, summary.low, summary.fixable
            )),
        }
        let mut new: Vec<&String> = ids.iter().filter(|i| !known.contains(*i)).collect();
        if !new.is_empty() {
            new.sort();
            let data = json!({ "critical": summary.critical, "high": summary.high, "new": new.len(), "ids": new.iter().take(5).collect::<Vec<_>>() });
            let (sid, e, d) = (service.to_string(), env.to_string(), dep.to_string());
            if let Err(err) = self.db(move |s| s.add_event("deploy.vulnerable", &sid, &e, Some(&d), &data)).await {
                tracing::warn!(service, deployment = dep, error = %err, "apps: a scan's deploy.vulnerable wasn't recorded");
            }
        }
        Some(summary)
    }

    /// `run.block_on` before a rollout: an error that fails the deploy when its image has a
    /// known vulnerability at `floor` or worse, or couldn't be scanned (a hold that can't look
    /// holds).
    pub(crate) async fn hold_on_vulnerabilities(self: &Arc<Self>, service: &str, env: &str, dep: &str, image: &str, floor: Severity, log: &LogSink) -> anyhow::Result<()> {
        log.line(format!("scanning the image for known vulnerabilities first (run.block_on: {})", severity_name(floor)));
        match self.scan_deployment(service, env, dep, image, log).await {
            None => anyhow::bail!("run.block_on holds a deploy until its image is scanned, and this node's builder doesn't scan images"),
            Some(ScanSummary { error: Some(why), .. }) => anyhow::bail!("run.block_on holds a deploy until its image is scanned, and the scan didn't finish: {why}"),
            Some(s) => {
                let n = s.at_least(floor);
                anyhow::ensure!(n == 0, "its image has {n} known {} at {} or worse (run.block_on): see its vulnerabilities", if n == 1 { "vulnerability" } else { "vulnerabilities" }, severity_name(floor));
                Ok(())
            }
        }
    }

    /// Owned services' live deployments, scanned again once a day (new advisories, the same
    /// image), one after another on a task of its own.
    pub(crate) async fn rescan_pass(self: &Arc<Self>, services: &[crate::store::ServiceRow], now: i64) {
        use std::sync::atomic::Ordering;
        if (now - self.rescan_looked.load(Ordering::SeqCst)).abs() < LOOK_MS || self.rescanning.swap(true, Ordering::SeqCst) {
            return;
        }
        self.rescan_looked.store(now, Ordering::SeqCst);
        let Ok(all_envs) = self.db(|s| s.all_environments()).await else {
            self.rescanning.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let mut due = Vec::new();
        for svc in services.iter().filter(|s| self.owns(&s.id)) {
            let sid = svc.id.clone();
            let Ok(sums) = self.db(move |s| s.scan_summaries(&sid)).await else { continue };
            for e in all_envs.iter().filter(|e| e.service == svc.id) {
                // Only a deployment scanned before (a builder that scans) is scanned again.
                if let Some(active) = &e.active
                    && sums.get(active).is_some_and(|s| now - s.scanned_at >= RESCAN_MS)
                {
                    due.push((svc.id.clone(), e.name.clone(), active.clone()));
                }
            }
        }
        let this = self.clone();
        tokio::spawn(async move {
            for (svc, env, dep) in due {
                let d = dep.clone();
                if let Ok(row) = this.db(move |s| s.deployment(&d)).await
                    && let Some(image) = row.image.filter(|_| row.pruned_at.is_none())
                {
                    let log = this.logs.sink(format!("env:{svc}/{env}"), Stream::System);
                    this.scan_deployment(&svc, &env, &dep, &image, &log).await;
                }
            }
            this.rescanning.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// `GET …/deploys/{dep}/vulnerabilities`: its last scan and what it found, most severe first;
    /// `severity` keeps those at it or worse.
    pub async fn vulnerabilities(self: &Arc<Self>, service: &str, env: &str, dep: &str, severity: Option<&str>) -> Result<Value> {
        let floor = match severity {
            None | Some("") => Severity::Unknown,
            Some(s) => match Severity::parse(s) {
                Severity::Unknown if s != "unknown" => return Err(Error::BadRequest("severity is critical, high, medium, low or unknown".into())),
                f => f,
            },
        };
        self.deployment(service, env, dep).await?;
        let d = dep.to_string();
        let got = self.db(move |s| s.scan(&d)).await?;
        Ok(match got {
            None => json!({ "scan": null, "vulnerabilities": [] }),
            Some((sum, items)) => json!({ "scan": sum, "vulnerabilities": items.into_iter().filter(|v| v.severity >= floor).collect::<Vec<_>>() }),
        })
    }

    /// Deployments with their scans' counts.
    pub(crate) async fn with_scans(self: &Arc<Self>, service: &str, mut deps: Vec<crate::model::Deployment>) -> Result<Vec<crate::model::Deployment>> {
        let sid = service.to_string();
        let mut sums = self.db(move |s| s.scan_summaries(&sid)).await?;
        for d in &mut deps {
            d.vulnerabilities = sums.remove(&d.id);
        }
        Ok(deps)
    }
}

pub fn severity_name(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
        Severity::Unknown => "unknown",
    }
}
