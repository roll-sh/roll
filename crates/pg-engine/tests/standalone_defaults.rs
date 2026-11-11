//! The shipped executable's default policy and migration errors, through a real SQL client.
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio_postgres::{Client, NoTls};

struct Server {
    child: Child,
    port: u16,
    log: std::path::PathBuf,
}

impl Server {
    fn start(data: &Path, extra: &[&str]) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let log = data.join("standalone.log");
        let output = std::fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_roll-engine"))
            .args([
                "--data",
                data.to_str().unwrap(),
                "--listen",
                &format!("127.0.0.1:{port}"),
            ])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(output)
            .spawn()
            .unwrap();
        Self { child, port, log }
    }

    async fn connect(&mut self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok((client, connection)) = tokio_postgres::connect(
                &format!(
                    "host=127.0.0.1 port={} user=postgres dbname=postgres",
                    self.port
                ),
                NoTls,
            )
            .await
            {
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                return client;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "{}",
                std::fs::read_to_string(&self.log).unwrap()
            );
            assert!(Instant::now() < deadline, "standalone startup timed out");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn stop(&mut self) -> std::process::ExitStatus {
        // SAFETY: signal only the child this test owns; no process-group or shared PID.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                self.child.kill().unwrap();
                return self.child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop();
        }
    }
}

#[tokio::test]
async fn fresh_restart_and_sql_settings() {
    let dir = tempfile::tempdir().unwrap();
    for restart in [false, true] {
        let mut server = Server::start(dir.path(), &[]);
        let client = server.connect().await;
        for setting in ["fsync", "synchronous_commit"] {
            let value: String = client
                .query_one(&format!("SHOW {setting}"), &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(value, "on");
        }
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_settings WHERE name = 'roll.engine_mode'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0);
        for sql in [
            "SET roll.engine_mode = 'efficient'",
            "SET roll.engine_mode = 'performance'",
            "ALTER SYSTEM SET roll.engine_mode = 'efficient'",
            "ALTER SYSTEM SET roll.engine_mode = 'performance'",
        ] {
            let error = client.batch_execute(sql).await.unwrap_err();
            let db = error.as_db_error().unwrap();
            assert_eq!(db.code().code(), "22023");
            assert!(db.message().contains("roll.engine_mode has been removed"));
        }
        let mut config = tokio_postgres::Config::new();
        config
            .host("127.0.0.1")
            .port(server.port)
            .user("postgres")
            .dbname("postgres")
            .options("-c roll.engine_mode=efficient");
        let error = config
            .connect(NoTls)
            .await
            .err()
            .expect("removed startup setting accepted");
        assert!(
            error
                .as_db_error()
                .unwrap()
                .message()
                .contains("roll.engine_mode has been removed")
        );
        // Other custom settings and persisted GUCs remain usable.
        client
            .batch_execute("SET app.example = 'ok'")
            .await
            .unwrap();
        client
            .batch_execute("ALTER SYSTEM SET work_mem = '8MB'")
            .await
            .unwrap();
        let value: String = client
            .query_one("SHOW app.example", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(value, "ok");
        if restart {
            let row = client
                .query_one("SELECT id, payload FROM default_durable", &[])
                .await
                .unwrap();
            assert_eq!(row.get::<_, i32>(0), 7);
            assert_eq!(row.get::<_, String>(1), "retained");
            let work_mem: String = client.query_one("SHOW work_mem", &[]).await.unwrap().get(0);
            assert_eq!(work_mem, "8MB");
        } else {
            client.batch_execute("CREATE TABLE default_durable(id int PRIMARY KEY, payload text); INSERT INTO default_durable VALUES (7, 'retained')").await.unwrap();
        }
        drop(client);
        assert!(server.stop().success());
    }
    for value in ["efficient", "performance"] {
        std::fs::write(
            dir.path().join(pg_engine::sysconf::AUTO_NAME),
            format!("roll.engine_mode = '{value}'\n"),
        )
        .unwrap();
        let mut server = Server::start(dir.path(), &[]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "retired selector did not fail startup"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert!(!status.success());
        assert!(
            std::fs::read_to_string(&server.log)
                .unwrap()
                .contains("roll.engine_mode has been removed")
        );
    }
}
