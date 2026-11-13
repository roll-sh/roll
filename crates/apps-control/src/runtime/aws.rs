//! What roll apps needs from AWS on the fleet (docs/apps/README.md, "The runtime" and "Builds"):
//! the settings [`super::fargate::Fargate`] and [`super::codebuild::CodeBuild`] share, the SDK
//! configuration and the stack's bucket.

use anyhow::{Context, anyhow, bail};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path as ObjPath;
use object_store::{ObjectStoreExt, PutPayload, WriteMultipart};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tracing::warn;

/// Keys for endpoints of their own (tests). `Debug` doesn't show them.
#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(…)")
    }
}

#[derive(Clone, Debug)]
pub struct FleetConfig {
    /// The ECS cluster (name or ARN).
    pub cluster: String,
    /// Where instances run: the isolated subnets, and a security group that admits fleet nodes
    /// on any TCP port (the app's port is the service's choice).
    pub subnets: Vec<String>,
    pub security_group: String,
    /// A public IP per task: its only way out in subnets without a NAT gateway.
    pub public_ip: bool,
    /// The role ECS pulls images, reads environment files and writes logs with.
    pub execution_role: String,
    /// The role the app's code gets; none: no AWS credentials in the task at all.
    pub task_role: Option<String>,
    /// The CloudWatch log group instances log to (stream prefix `apps`).
    pub log_group: String,
    /// `ARM64` or `X86_64`: what the CodeBuild project builds for.
    pub arch: String,
    /// The CodeBuild project builds run in (privileged, its role may push to `ecr_repo/*`, read
    /// `<prefix>builds/*` and the build secrets).
    pub codebuild_project: String,
    /// The ECR repository prefix: a service's images are `<ecr_repo>/<service>`.
    pub ecr_repo: String,
    /// The stack's bucket: `<prefix>env/…` (instances' environment files) and
    /// `<prefix>builds/…` (build contexts), each deleted once read.
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
    /// ECS, CodeBuild, ECR, CloudWatch Logs and Secrets Manager at another address (tests).
    pub endpoint: Option<String>,
    /// S3 at another address (tests, MinIO).
    pub s3_endpoint: Option<String>,
    /// Every image registry at this base URL, private addresses allowed (tests).
    pub registry: Option<String>,
    /// Keys for those endpoints; otherwise the usual AWS chain (the node's task role).
    pub credentials: Option<Credentials>,
    /// Where build contexts are packed before they're uploaded.
    pub scratch: PathBuf,
    /// How often a starting or running task, a build and their logs are looked at.
    pub poll: Duration,
    /// How long a task may take to reach RUNNING (the image pull included).
    pub start_timeout: Duration,
}

impl FleetConfig {
    /// `None` unless `ROLL_APPS_RUNTIME=fargate` (`docker`, the default, runs and builds
    /// everything on this host). Settings: `ROLL_APPS_FARGATE_CLUSTER`, `_SUBNETS` (comma
    /// separated), `_SECURITY_GROUP`, `_EXECUTION_ROLE`, `_TASK_ROLE` (optional), `_LOG_GROUP`,
    /// `_PUBLIC_IP` (`1`), `_ARCH` (`ARM64`), `ROLL_APPS_CODEBUILD_PROJECT`, `ROLL_APPS_ECR_REPO`
    /// (`apps`), `ROLL_APPS_BUCKET` (default `ROLL_S3_BUCKET`), `ROLL_APPS_PREFIX` (`apps/`), and
    /// `AWS_REGION`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>, scratch: PathBuf) -> anyhow::Result<Option<FleetConfig>> {
        let get = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        match get("ROLL_APPS_RUNTIME").as_deref() {
            None | Some("docker") => return Ok(None),
            Some("fargate") => {}
            Some(other) => bail!("ROLL_APPS_RUNTIME is docker or fargate, not {other}"),
        }
        let need = |k: &str| get(k).ok_or_else(|| anyhow!("ROLL_APPS_RUNTIME=fargate needs {k}"));
        let subnets: Vec<String> = need("ROLL_APPS_FARGATE_SUBNETS")?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let arch = get("ROLL_APPS_FARGATE_ARCH").unwrap_or_else(|| "ARM64".into()).to_ascii_uppercase();
        if arch != "ARM64" && arch != "X86_64" {
            bail!("ROLL_APPS_FARGATE_ARCH is ARM64 or X86_64, not {arch}");
        }
        let mut prefix = get("ROLL_APPS_PREFIX").unwrap_or_else(|| "apps/".into());
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Some(FleetConfig {
            cluster: need("ROLL_APPS_FARGATE_CLUSTER")?,
            subnets,
            security_group: need("ROLL_APPS_FARGATE_SECURITY_GROUP")?,
            public_ip: get("ROLL_APPS_FARGATE_PUBLIC_IP").is_some_and(|v| v == "1" || v == "true"),
            execution_role: need("ROLL_APPS_FARGATE_EXECUTION_ROLE")?,
            task_role: get("ROLL_APPS_FARGATE_TASK_ROLE"),
            log_group: need("ROLL_APPS_FARGATE_LOG_GROUP")?,
            arch,
            codebuild_project: need("ROLL_APPS_CODEBUILD_PROJECT")?,
            ecr_repo: get("ROLL_APPS_ECR_REPO").unwrap_or_else(|| "apps".into()).trim_matches('/').to_string(),
            bucket: get("ROLL_APPS_BUCKET").or_else(|| get("ROLL_S3_BUCKET")).ok_or_else(|| anyhow!("ROLL_APPS_RUNTIME=fargate needs ROLL_APPS_BUCKET or ROLL_S3_BUCKET"))?,
            prefix,
            region: get("AWS_REGION").or_else(|| get("AWS_DEFAULT_REGION")),
            endpoint: None,
            s3_endpoint: None,
            registry: None,
            credentials: None,
            scratch,
            poll: Duration::from_secs(2),
            start_timeout: Duration::from_secs(300),
        }))
    }

    fn region(&self) -> Option<String> {
        self.region.clone().or_else(|| self.endpoint.as_ref().map(|_| "us-east-1".to_string()))
    }

    /// The ARN of an object in the bucket (ECS takes environment files by ARN).
    pub(crate) fn object_arn(&self, key: &str) -> String {
        let partition = match self.region.as_deref() {
            Some(r) if r.starts_with("cn-") => "aws-cn",
            Some(r) if r.starts_with("us-gov-") => "aws-us-gov",
            _ => "aws",
        };
        format!("arn:{partition}:s3:::{}/{key}", self.bucket)
    }

    /// The SDK configuration every client but S3's is made from.
    pub(crate) async fn sdk(&self) -> aws_config::SdkConfig {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(3))
            .timeout_config(aws_config::timeout::TimeoutConfig::builder().operation_timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(5)).build());
        if let Some(r) = self.region() {
            loader = loader.region(aws_config::Region::new(r));
        }
        if let Some(e) = &self.endpoint {
            loader = loader.endpoint_url(e);
        }
        if let Some(c) = &self.credentials {
            loader = loader.credentials_provider(aws_sdk_ecs::config::Credentials::new(&c.access_key, &c.secret_key, None, None, "roll"));
        }
        loader.load().await
    }

    pub(crate) fn bucket(&self) -> anyhow::Result<Bucket> {
        let mut b = AmazonS3Builder::from_env().with_bucket_name(&self.bucket);
        if let Some(r) = self.region() {
            b = b.with_region(r);
        }
        if let Some(e) = &self.s3_endpoint {
            b = b.with_endpoint(e).with_allow_http(e.starts_with("http://"));
        }
        if let Some(c) = &self.credentials {
            b = b.with_access_key_id(&c.access_key).with_secret_access_key(&c.secret_key);
        }
        Ok(Bucket(b.build().context("the apps bucket")?))
    }
}

/// The stack's bucket.
pub(crate) struct Bucket(AmazonS3);

/// Files up to this size go up in one PUT.
const SINGLE_PUT: u64 = 64 << 20;

impl Bucket {
    pub(crate) async fn put(&self, key: &str, data: Vec<u8>) -> anyhow::Result<()> {
        self.0.put(&ObjPath::from(key), PutPayload::from(data)).await.with_context(|| format!("uploading s3://…/{key}"))?;
        Ok(())
    }

    pub(crate) async fn put_file(&self, key: &str, file: &Path) -> anyhow::Result<()> {
        let size = tokio::fs::metadata(file).await?.len();
        if size <= SINGLE_PUT {
            return self.put(key, tokio::fs::read(file).await?).await;
        }
        let mut writer = WriteMultipart::new_with_chunk_size(self.0.put_multipart(&ObjPath::from(key)).await?, 8 << 20);
        let mut f = tokio::fs::File::open(file).await?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            writer.wait_for_capacity(4).await?;
            writer.write(&buf[..n]);
        }
        writer.finish().await.with_context(|| format!("uploading s3://…/{key}"))?;
        Ok(())
    }

    pub(crate) async fn delete(&self, key: &str) {
        match self.0.delete(&ObjPath::from(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => warn!(key, error = %e, "apps: deleting an object"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fleet_runtime_is_chosen_and_checked_from_its_settings() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert!(FleetConfig::from_lookup(env(&[]), PathBuf::new()).unwrap().is_none());
        assert!(FleetConfig::from_lookup(env(&[("ROLL_APPS_RUNTIME", "docker")]), PathBuf::new()).unwrap().is_none());
        assert!(FleetConfig::from_lookup(env(&[("ROLL_APPS_RUNTIME", "k8s")]), PathBuf::new()).is_err());
        let err = FleetConfig::from_lookup(env(&[("ROLL_APPS_RUNTIME", "fargate")]), PathBuf::new()).unwrap_err();
        assert!(err.to_string().contains("ROLL_APPS_FARGATE_SUBNETS"), "{err}");
        let c = FleetConfig::from_lookup(
            env(&[
                ("ROLL_APPS_RUNTIME", "fargate"),
                ("ROLL_APPS_FARGATE_CLUSTER", "roll-cloud"),
                ("ROLL_APPS_FARGATE_SUBNETS", "subnet-a, subnet-b"),
                ("ROLL_APPS_FARGATE_SECURITY_GROUP", "sg-apps"),
                ("ROLL_APPS_FARGATE_EXECUTION_ROLE", "arn:aws:iam::1:role/apps-exec"),
                ("ROLL_APPS_FARGATE_LOG_GROUP", "/roll-cloud/apps"),
                ("ROLL_APPS_CODEBUILD_PROJECT", "roll-cloud-apps"),
                ("ROLL_S3_BUCKET", "data"),
                ("AWS_REGION", "us-gov-west-1"),
            ]),
            PathBuf::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!((c.subnets.len(), c.public_ip, c.arch.as_str(), c.ecr_repo.as_str(), c.bucket.as_str(), c.prefix.as_str()), (2, false, "ARM64", "apps", "data", "apps/"));
        assert_eq!(c.object_arn("apps/env/x.env"), "arn:aws-us-gov:s3:::data/apps/env/x.env");
    }
}
