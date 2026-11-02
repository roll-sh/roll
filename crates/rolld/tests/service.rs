//! Drives the daemon executable through HTTP and the PostgreSQL proxy, including process loss.

use anyhow::{Context, Result, bail};
use pg_storage::Lsn;
use serde_json::{Value, json};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::{Client, NoTls};

struct Daemon {
    child: Child,
    home: PathBuf,
    api: SocketAddr,
    proxy: SocketAddr,
    http: reqwest::Client,
    passed: bool,
}

fn cleanup_docker(args: &[&str]) -> Result<Output> {
    let mut child = Command::new("docker")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Docker cleanup command exceeded 5 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Daemon {
    fn spawn(home: &Path, api: SocketAddr, proxy: SocketAddr) -> Result<Child> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("rolld.log"))?;
        Ok(Command::new(env!("CARGO_BIN_EXE_rolld"))
            .arg("--home")
            .arg(home)
            .args(["--api", &api.to_string(), "--proxy", &proxy.to_string()])
            .env_remove("ROLL_S3_BUCKET")
            .env_remove("ROLL_DEFAULT_ENDPOINT")
            .env("ROLL_PUBLIC_HOST", "127.0.0.1")
            .env("ROLL_MEM_PER_CU", "1024")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?)
    }

    async fn start() -> Result<Self> {
        let api = TcpListener::bind("127.0.0.1:0")?;
        let proxy = TcpListener::bind("127.0.0.1:0")?;
        let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("rolld-service-{}", std::process::id()));
        std::fs::create_dir(&home)?;
        let (api_addr, proxy_addr) = (api.local_addr()?, proxy.local_addr()?);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        drop((api, proxy));
        let mut daemon = Self {
            child: Self::spawn(&home, api_addr, proxy_addr)?,
            home,
            api: api_addr,
            proxy: proxy_addr,
            http,
            passed: false,
        };
        daemon.ready().await?;
        Ok(daemon)
    }

    async fn ready(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(status) = self.child.try_wait()? {
                    bail!(
                        "rolld exited {status}; see {}",
                        self.home.join("rolld.log").display()
                    );
                }
                if let Ok(r) = self
                    .http
                    .get(format!("http://{}/v1/health", self.api))
                    .send()
                    .await
                    && r.status().is_success()
                    && r.json::<Value>().await?["ok"] == true
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await?
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Value) -> Result<Value> {
        Ok(self
            .http
            .request(method, format!("http://{}{path}", self.api))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn connect(&self, endpoint: &str, password: &str) -> Result<Client> {
        let (client, connection) = tokio_postgres::Config::new()
            .host("127.0.0.1")
            .port(self.proxy.port())
            .user("roll")
            .password(password)
            .dbname("main")
            .options(format!("endpoint={endpoint}"))
            .connect_timeout(Duration::from_secs(120))
            .connect(NoTls)
            .await?;
        tokio::spawn(connection);
        Ok(client)
    }

    async fn cpu_limit(&self, endpoint: &str) -> Result<u64> {
        let mut command = tokio::process::Command::new("docker");
        command
            .args([
                "inspect",
                "--format",
                "{{.HostConfig.NanoCpus}}",
                &format!("roll-{endpoint}"),
            ])
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(5), command.output()).await??;
        anyhow::ensure!(out.status.success(), "inspect compute CPU limit");
        Ok(String::from_utf8(out.stdout)?.trim().parse()?)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let filter = format!("label=roll.home={}", self.home.display());
        let cleanup = (|| -> Result<()> {
            let out = cleanup_docker(&["ps", "-aq", "--filter", &filter])?;
            anyhow::ensure!(out.status.success(), "list owned computes during cleanup");
            let ids = String::from_utf8(out.stdout)?;
            let ids: Vec<_> = ids.split_whitespace().collect();
            if !ids.is_empty() {
                let mut args = vec!["rm", "-f"];
                args.extend(ids);
                anyhow::ensure!(
                    cleanup_docker(&args)?.status.success(),
                    "remove owned computes"
                );
            }
            Ok(())
        })();
        if self.passed {
            assert!(cleanup.is_ok(), "{cleanup:?}");
            if std::env::var_os("ROLL_E2E_KEEP").is_none() {
                std::fs::rename(self.home.join("rolld.log"), self.home.with_extension("log"))
                    .expect("keep daemon log");
                std::fs::remove_dir_all(&self.home).expect("remove successful test state");
            }
        } else if let Err(error) = cleanup {
            eprintln!(
                "rolld test cleanup failed: {error}; state at {}",
                self.home.display()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker and postgres:18-alpine; run with ROLL_E2E=1"]
async fn serves_requests_and_adopts_compute_after_restart() -> Result<()> {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        bail!("set ROLL_E2E=1 to run the actual-daemon test");
    }
    tokio::time::timeout(Duration::from_secs(240), async {
        let mut d = Daemon::start().await?;
        let project = d
            .request(
                reqwest::Method::POST,
                "/v1/projects",
                json!({"name":"daemon-service", "pg_version":18}),
            )
            .await?;
        let endpoint = project["branches"][0]["endpoint"]["id"]
            .as_str()
            .context("endpoint")?;
        let branch = project["branches"][0]["id"].as_str().context("branch")?;
        let password = project["password"].as_str().context("password")?;
        let project_id = project["id"].as_str().context("project")?;
        d.request(
            reqwest::Method::PATCH,
            &format!("/v1/endpoints/{endpoint}"),
            json!({"min_cu":0.25,"max_cu":0.25,"suspend_timeout_s":0}),
        )
        .await?;
        let client = d.connect(endpoint, password).await?;
        assert_eq!(d.cpu_limit(endpoint).await?, 250000000);
        client
            .batch_execute("CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL)")
            .await?;
        client
            .batch_execute("BEGIN; INSERT INTO t VALUES (0, 0); ROLLBACK")
            .await?;
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM t", &[])
                .await?
                .get::<_, i64>(0),
            0
        );
        let postmaster: String = client
            .query_one("SELECT pg_postmaster_start_time()::text", &[])
            .await?
            .get(0);
        let mut clients = Vec::new();
        for _ in 0..4 {
            clients.push(d.connect(endpoint, password).await?);
        }
        let written = Arc::new(tokio::sync::Barrier::new(4));
        let mut workers = tokio::task::JoinSet::new();
        for (worker, c) in (0..4i32).zip(clients) {
            let written = written.clone();
            workers.spawn(async move {
                c.batch_execute("BEGIN").await?;
                let rows = c
                    .execute(
                        "INSERT INTO t SELECT g, g * 3 FROM generate_series($1::int, $2::int) g",
                        &[&(worker * 25 + 1), &(worker * 25 + 25)],
                    )
                    .await?;
                written.wait().await;
                c.batch_execute("COMMIT").await?;
                Ok::<_, tokio_postgres::Error>(rows)
            });
        }
        while let Some(result) = workers.join_next().await {
            assert_eq!(result??, 25);
        }
        let backend: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        d.request(
            reqwest::Method::PATCH,
            &format!("/v1/endpoints/{endpoint}"),
            json!({"min_cu":0.5,"max_cu":0.5,"suspend_timeout_s":0}),
        )
        .await?;
        assert_eq!(d.cpu_limit(endpoint).await?, 500000000);
        assert_eq!(
            client
                .query_one("SELECT pg_backend_pid()", &[])
                .await?
                .get::<_, i32>(0),
            backend
        );
        drop(client);
        d.request(
            reqwest::Method::POST,
            &format!("/v1/endpoints/{endpoint}/suspend"),
            json!({}),
        )
        .await?;
        let client = d.connect(endpoint, password).await?;
        let row = client
            .query_one("SELECT count(*), sum(v) FROM t", &[])
            .await?;
        assert_eq!((row.get::<_, i64>(0), row.get::<_, i64>(1)), (100, 15150));
        assert_ne!(
            client
                .query_one("SELECT pg_postmaster_start_time()::text", &[])
                .await?
                .get::<_, String>(0),
            postmaster
        );
        let resumed: String = client
            .query_one("SELECT pg_postmaster_start_time()::text", &[])
            .await?
            .get(0);
        drop(client);

        d.child.kill()?;
        d.child.wait()?;
        d.child = Daemon::spawn(&d.home, d.api, d.proxy)?;
        d.ready().await?;
        let client = d.connect(endpoint, password).await?;
        assert_eq!(
            client
                .query_one("SELECT pg_postmaster_start_time()::text", &[])
                .await?
                .get::<_, String>(0),
            resumed
        );
        client
            .execute("INSERT INTO t VALUES ($1, $2)", &[&101i32, &303i32])
            .await?;
        let row = client
            .query_one("SELECT count(*), sum(v) FROM t", &[])
            .await?;
        assert_eq!((row.get::<_, i64>(0), row.get::<_, i64>(1)), (101, 15453));
        let lsn: Lsn = client
            .query_one("SELECT pg_current_wal_lsn()::text", &[])
            .await?
            .get::<_, String>(0)
            .parse()?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let history = d
                    .request(
                        reqwest::Method::GET,
                        &format!("/v1/projects/{project_id}/branches/{branch}/history"),
                        Value::Null,
                    )
                    .await?;
                if let Some(flush) = history["flush_lsn"].as_str()
                    && flush.parse::<Lsn>()? >= lsn
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await??;
        d.passed = true;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
