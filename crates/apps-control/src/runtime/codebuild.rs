//! Builds in AWS CodeBuild (docs/apps/README.md, "Builds"): a build is customer code, so on the
//! fleet it never runs on a node. It runs in CodeBuild's own VM (privileged, for BuildKit) and
//! pushes to the service's ECR repository, `<ecr_repo>/<service>`.
//!
//! - The context goes up as a zip (CodeBuild's S3 source takes zips) to
//!   `<prefix>builds/<deployment>-<random>.zip` in the stack's bucket, deleted when the build
//!   ends. Symlinks stay symlinks; nothing outside the context is read.
//! - `StartBuild` on the configured project with that object as an S3 source and an inline
//!   buildspec: log in to ECR, then `docker buildx build` (BuildKit in a `docker-container`
//!   builder) with the job's Dockerfile, a registry build cache
//!   (`--cache-from`/`--cache-to type=registry,ref=<repo>:buildcache`, ECR's OCI form), and
//!   `--push` of `<repo>:<deployment>`. Everything in the buildspec is roll's: names, the image
//!   and the Dockerfile's path, shell-quoted.
//! - **Variables** reach the build only through Secrets Manager: one secret per build
//!   (`roll-apps/builds/<service>-<deployment>-<random>`, a JSON object of the values), and each
//!   variable an environment override of type `SECRETS_MANAGER` naming `<secret>:<NAME>`.
//!   CodeBuild resolves them inside the build; no value is in the `StartBuild` request, the
//!   buildspec or the build's record. They're BuildKit secrets (`--secret id=K,env=K`) and, for
//!   the tree's own Dockerfile, build arguments read from the environment (`--build-arg K`), as
//!   on Docker. The secret is deleted (without recovery) when the build ends. Names CodeBuild or
//!   the AWS CLI read (`CODEBUILD_*`, `AWS_*`) aren't passed, and the log says so.
//! - `BatchGetBuilds` is polled until the build is done, its CloudWatch log followed into the
//!   build's log; `job.timeout` is the build's timeout (and a local deadline), a cancel is
//!   `StopBuild`.
//! - [`Builder::remove_image`] is ECR `BatchDeleteImage` of the deployment's tag; the repository
//!   and its `buildcache` tag stay.

use super::aws::{Bucket, FleetConfig};
use super::{BuildJob, Builder};
use crate::logs::{LogSink, Stream};
use anyhow::{Context, bail};
use async_trait::async_trait;
use aws_sdk_codebuild::types::{Build, EnvironmentVariable, EnvironmentVariableType, SourceType, StatusType};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// The tag of a service's registry build cache.
pub const CACHE_TAG: &str = "buildcache";
/// Secrets Manager holds at most 64 KiB in a secret.
const SECRET_MAX: usize = 64 << 10;

pub struct CodeBuild {
    cfg: FleetConfig,
    codebuild: aws_sdk_codebuild::Client,
    ecr: aws_sdk_ecr::Client,
    secrets: aws_sdk_secretsmanager::Client,
    logs: aws_sdk_cloudwatchlogs::Client,
    bucket: Bucket,
}

/// `'…'` for `sh`.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Names CodeBuild or the AWS CLI in the build read themselves.
fn reserved(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.starts_with("CODEBUILD_") || n.starts_with("AWS_")
}

/// The buildspec of one build (YAML; each command a JSON string, which YAML reads as a quoted
/// scalar).
pub fn buildspec(registry: &str, repo_uri: &str, tag: &str, dockerfile: &str, names: &[String], own_dockerfile: bool, env_hash: &str) -> String {
    let image = format!("{repo_uri}:{tag}");
    let cache = format!("{repo_uri}:{CACHE_TAG}");
    let mut build = vec![
        "docker", "buildx", "build", "--progress", "plain", "--provenance", "false", "--file",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    build.push(quote(dockerfile));
    build.extend(["--tag".into(), quote(&image), "--label".into(), quote(&format!("{}={tag}", super::LABEL))]);
    build.extend(["--cache-from".into(), quote(&format!("type=registry,ref={cache}"))]);
    build.extend(["--cache-to".into(), quote(&format!("type=registry,ref={cache},mode=max,image-manifest=true,oci-mediatypes=true"))]);
    for k in names {
        build.push("--secret".into());
        build.push(quote(&format!("id={k},env={k}")));
        if own_dockerfile {
            build.push("--build-arg".into());
            build.push(quote(k));
        }
    }
    if !names.is_empty() && !own_dockerfile {
        build.push("--build-arg".into());
        build.push(quote(&format!("ROLL_BUILD_ENV={env_hash}")));
    }
    build.extend(["--push".into(), ".".into()]);
    let pre = [
        format!("aws ecr get-login-password --region \"$AWS_REGION\" | docker login --username AWS --password-stdin {}", quote(registry)),
        "docker buildx create --name roll --driver docker-container --use".to_string(),
    ];
    let line = |c: &str| format!("      - {}\n", serde_json::Value::String(c.to_string()));
    let mut out = String::from("version: 0.2\nenv:\n  shell: bash\nphases:\n  pre_build:\n    commands:\n");
    for c in &pre {
        out.push_str(&line(c));
    }
    out.push_str("  build:\n    commands:\n");
    out.push_str(&line(&build.join(" ")));
    out
}

/// Zip `dir` to `out`: files with their modes, symlinks as symlinks, nothing followed.
fn zip_dir(dir: &Path, out: &Path) -> anyhow::Result<()> {
    use zip::write::SimpleFileOptions;
    let mut z = zip::ZipWriter::new(std::io::BufWriter::new(std::fs::File::create(out)?));
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((path, rel)) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&path)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        if entries.is_empty() && !rel.is_empty() {
            z.add_directory(&rel, SimpleFileOptions::default())?;
        }
        for e in entries {
            let name = e.file_name().into_string().map_err(|n| anyhow::anyhow!("{n:?} isn't UTF-8"))?;
            let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let meta = std::fs::symlink_metadata(e.path())?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                let target = std::fs::read_link(e.path())?;
                z.add_symlink(&rel, target.to_string_lossy(), SimpleFileOptions::default())?;
            } else if ft.is_dir() {
                stack.push((e.path(), rel));
            } else if ft.is_file() {
                use std::os::unix::fs::PermissionsExt;
                let opts = SimpleFileOptions::default().unix_permissions(meta.permissions().mode() & 0o777).large_file(meta.len() >= u32::MAX as u64);
                z.start_file(&rel, opts)?;
                std::io::copy(&mut std::fs::File::open(e.path())?, &mut z)?;
            }
        }
    }
    z.finish()?.flush()?;
    Ok(())
}

/// Why a finished build failed, from its phases.
fn failure(b: &Build) -> String {
    let status = b.build_status().map(|s| s.as_str()).unwrap_or("FAILED");
    let phase = b.phases().iter().rev().find(|p| p.phase_status().is_some_and(|s| *s != StatusType::Succeeded));
    match phase {
        Some(p) => {
            let msg: Vec<&str> = p.contexts().iter().filter_map(|c| c.message()).filter(|m| !m.is_empty()).collect();
            format!("{status} in {}{}", p.phase_type().map(|t| t.as_str()).unwrap_or("?"), if msg.is_empty() { String::new() } else { format!(": {}", msg.join("; ")) })
        }
        None => status.to_string(),
    }
}

impl CodeBuild {
    pub async fn new(cfg: FleetConfig) -> anyhow::Result<Arc<CodeBuild>> {
        std::fs::create_dir_all(&cfg.scratch).with_context(|| format!("creating {}", cfg.scratch.display()))?;
        let sdk = cfg.sdk().await;
        Ok(Arc::new(CodeBuild {
            codebuild: aws_sdk_codebuild::Client::new(&sdk),
            ecr: aws_sdk_ecr::Client::new(&sdk),
            secrets: aws_sdk_secretsmanager::Client::new(&sdk),
            logs: aws_sdk_cloudwatchlogs::Client::new(&sdk),
            bucket: cfg.bucket()?,
            cfg,
        }))
    }

    /// The service's repository's URI, made if it's missing.
    async fn repository(&self, service: &str) -> anyhow::Result<String> {
        let name = format!("{}/{}", self.cfg.ecr_repo, service.strip_prefix("svc_").unwrap_or(service));
        let made = self
            .ecr
            .create_repository()
            .repository_name(&name)
            .tags(aws_sdk_ecr::types::Tag::builder().key(super::LABEL).value(service).build()?)
            .send()
            .await;
        if let Ok(out) = &made
            && let Some(uri) = out.repository().and_then(|r| r.repository_uri())
        {
            return Ok(uri.to_string());
        }
        // There already (the usual case), or the create failed: the describe says which.
        let out = self.ecr.describe_repositories().repository_names(&name).send().await;
        match out {
            Ok(o) => o.repositories().first().and_then(|r| r.repository_uri()).map(str::to_string).with_context(|| format!("ECR has no repository {name}")),
            Err(e) => Err(anyhow::Error::new(e).context(match made {
                Err(c) => format!("making the ECR repository {name}: {}", aws_sdk_ecr::error::DisplayErrorContext(c)),
                Ok(_) => format!("reading the ECR repository {name}"),
            })),
        }
    }

    /// Follow a build's log from `token`; the next token.
    async fn log_page(&self, group: &str, stream: &str, token: Option<String>, log: &LogSink) -> Option<String> {
        let mut token = token;
        for _ in 0..16 {
            let r = self.logs.get_log_events().log_group_name(group).log_stream_name(stream).start_from_head(true).set_next_token(token.clone()).send().await;
            let Ok(r) = r else { return token };
            for e in r.events() {
                if let Some(m) = e.message() {
                    log.line(m.trim_end_matches(['\n', '\r']));
                }
            }
            let next = r.next_forward_token().map(str::to_string);
            let done = r.events().is_empty() || next.is_none() || next == token;
            if next.is_some() {
                token = next;
            }
            if done {
                break;
            }
        }
        token
    }

    async fn stop_build(&self, id: &str) {
        if let Err(e) = self.codebuild.stop_build().id(id).send().await {
            warn!(build = id, error = %aws_sdk_codebuild::error::DisplayErrorContext(e), "apps: StopBuild");
        }
    }

    /// Run the build to its end: the image.
    async fn run(&self, job: &BuildJob, log: &LogSink, cancel: &CancellationToken, source: &str, secret: Option<&str>, names: &[String]) -> anyhow::Result<String> {
        let repo_uri = self.repository(&job.service).await?;
        let registry = repo_uri.split('/').next().unwrap_or(&repo_uri).to_string();
        let tag = job.deployment.strip_prefix("dep_").unwrap_or(&job.deployment);
        let spec = buildspec(&registry, &repo_uri, tag, &job.dockerfile, names, job.own_dockerfile, &job.env_hash);
        let env: Vec<EnvironmentVariable> = match secret {
            Some(s) => names
                .iter()
                .map(|k| EnvironmentVariable::builder().name(k).value(format!("{s}:{k}")).r#type(EnvironmentVariableType::SecretsManager).build())
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        let minutes = (job.timeout.as_secs().div_ceil(60) as i32).clamp(5, 2160);
        let started = self
            .codebuild
            .start_build()
            .project_name(&self.cfg.codebuild_project)
            .source_type_override(SourceType::S3)
            .source_location_override(format!("{}/{source}", self.cfg.bucket))
            .buildspec_override(spec)
            .privileged_mode_override(true)
            .timeout_in_minutes_override(minutes)
            .set_environment_variables_override(Some(env).filter(|e| !e.is_empty()))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("StartBuild: {}", aws_sdk_codebuild::error::DisplayErrorContext(e)))?;
        let id = started.build_value().and_then(|b| b.id()).context("StartBuild returned no build")?.to_string();
        log.stream(Stream::System).line(format!("CodeBuild build {id}"));
        let deadline = Instant::now() + job.timeout;
        let mut token = None;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.cfg.poll) => {}
                _ = cancel.cancelled() => {
                    self.stop_build(&id).await;
                    bail!("canceled");
                }
            }
            let b = match self.codebuild.batch_get_builds().ids(&id).send().await {
                Ok(o) => o.builds().first().cloned(),
                Err(e) => {
                    warn!(build = %id, error = %aws_sdk_codebuild::error::DisplayErrorContext(e), "apps: BatchGetBuilds");
                    None
                }
            };
            if let Some(b) = &b
                && let Some(l) = b.logs()
                && let (Some(g), Some(s)) = (l.group_name(), l.stream_name())
            {
                token = self.log_page(g, s, token, log).await;
            }
            let done = b.as_ref().is_some_and(|b| b.build_complete());
            if done {
                let b = b.expect("seen");
                // The last lines can reach CloudWatch a moment after the build ended.
                if let Some(l) = b.logs()
                    && let (Some(g), Some(s)) = (l.group_name(), l.stream_name())
                {
                    tokio::time::sleep(self.cfg.poll).await;
                    self.log_page(g, s, token.clone(), log).await;
                }
                if b.build_status() == Some(&StatusType::Succeeded) {
                    return Ok(format!("{repo_uri}:{tag}"));
                }
                bail!("the build failed (CodeBuild: {})", failure(&b));
            }
            if Instant::now() >= deadline {
                self.stop_build(&id).await;
                bail!("the build took longer than {} minutes", job.timeout.as_secs() / 60);
            }
        }
    }
}

#[async_trait]
impl Builder for CodeBuild {
    fn name(&self) -> &'static str {
        "codebuild"
    }

    async fn build(&self, job: &BuildJob, log: &LogSink, cancel: CancellationToken) -> anyhow::Result<String> {
        let svc = job.service.strip_prefix("svc_").unwrap_or(&job.service);
        let id = format!("{svc}-{}-{}", job.deployment.strip_prefix("dep_").unwrap_or(&job.deployment), crate::model::random(6));
        // The variables, sealed in a secret of this build's own.
        let (skipped, vars): (Vec<_>, Vec<_>) = job.env.iter().partition(|(k, _)| reserved(k));
        if !skipped.is_empty() {
            let names: Vec<&str> = skipped.iter().map(|(k, _)| k.as_str()).collect();
            log.stream(Stream::System).line(format!("not passed to the build (CodeBuild reads these itself): {}", names.join(", ")));
        }
        let names: Vec<String> = vars.iter().map(|(k, _)| k.clone()).collect();
        let secret = if vars.is_empty() {
            None
        } else {
            let value = serde_json::Value::Object(vars.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect()).to_string();
            anyhow::ensure!(value.len() <= SECRET_MAX, "the build's variables are over 64 KiB, more than CodeBuild can take");
            let name = format!("roll-apps/builds/{id}");
            self.secrets
                .create_secret()
                .name(&name)
                .description("roll apps: one build's variables, deleted when it ends")
                .secret_string(value)
                .tags(aws_sdk_secretsmanager::types::Tag::builder().key(super::LABEL).value(&job.service).build())
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("storing the build's variables: {}", aws_sdk_secretsmanager::error::DisplayErrorContext(e)))?;
            Some(name)
        };
        let key = format!("{}builds/{id}.zip", self.cfg.prefix);
        let file = self.cfg.scratch.join(format!("{id}.zip"));
        let (ctx, f) = (job.context.clone(), file.clone());
        let packed = tokio::task::spawn_blocking(move || zip_dir(&ctx, &f)).await?;
        let r = match packed {
            Ok(()) => match self.bucket.put_file(&key, &file).await {
                Ok(()) => self.run(job, log, &cancel, &key, secret.as_deref(), &names).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e.context("packing the build context")),
        };
        let _ = tokio::fs::remove_file(&file).await;
        self.bucket.delete(&key).await;
        if let Some(name) = &secret
            && let Err(e) = self.secrets.delete_secret().secret_id(name).force_delete_without_recovery(true).send().await
        {
            warn!(secret = %name, error = %aws_sdk_secretsmanager::error::DisplayErrorContext(e), "apps: deleting a build's variables");
        }
        r
    }

    /// The deployment's tag goes; the image's layers go with ECR's own cleanup once no tag
    /// holds them.
    async fn remove_image(&self, image: &str) {
        let Some((uri, tag)) = image.rsplit_once(':').filter(|(u, t)| !t.contains('/') && u.contains('/')) else { return };
        let Some((_, repo)) = uri.split_once('/') else { return };
        if !repo.starts_with(&format!("{}/", self.cfg.ecr_repo)) || tag == CACHE_TAG {
            return;
        }
        if let Err(e) = self.ecr.batch_delete_image().repository_name(repo).image_ids(aws_sdk_ecr::types::ImageIdentifier::builder().image_tag(tag).build()).send().await {
            warn!(image, error = %aws_sdk_ecr::error::DisplayErrorContext(e), "apps: BatchDeleteImage");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buildspec_names_variables_and_quotes_what_it_takes() {
        let s = buildspec("1.dkr.ecr.us-east-1.amazonaws.com", "1.dkr.ecr.us-east-1.amazonaws.com/apps/web", "abc", "it's/Dockerfile", &["NPM_TOKEN".into()], false, "h1");
        assert!(s.starts_with("version: 0.2\n"), "{s}");
        assert!(s.contains(r#"'it'\\''s/Dockerfile'"#), "{s}");
        assert!(s.contains("--secret 'id=NPM_TOKEN,env=NPM_TOKEN'") && s.contains("'ROLL_BUILD_ENV=h1'") && !s.contains("--build-arg 'NPM_TOKEN'"), "{s}");
        assert!(s.contains("type=registry,ref=1.dkr.ecr.us-east-1.amazonaws.com/apps/web:buildcache,mode=max"), "{s}");
        assert!(s.contains("--tag '1.dkr.ecr.us-east-1.amazonaws.com/apps/web:abc'") && s.contains("--push ."), "{s}");
        let own = buildspec("r", "r/apps/web", "abc", "Dockerfile", &["NPM_TOKEN".into()], true, "h1");
        assert!(own.contains("--build-arg 'NPM_TOKEN'") && !own.contains("ROLL_BUILD_ENV"), "{own}");
        assert!(reserved("AWS_REGION") && reserved("codebuild_x") && !reserved("NPM_TOKEN"));
    }
}
