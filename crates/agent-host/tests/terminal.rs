//! Terminals: real PTYs through the daemon (input, output deltas, scrollback, resize, exit,
//! close, thread terminals).

mod common;

use agent_host::provider::fake::FakeProvider;
use agent_host::{Daemon, Outbound};
use agent_proto::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use common::*;
use std::time::Duration;
use tokio::sync::broadcast;

/// Collects a terminal's output until it contains `needle`.
async fn read_until(rx: &mut broadcast::Receiver<Outbound>, terminal: &str, seen: &mut String, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !seen.contains(needle) {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; got {seen:?}"))
            .unwrap();
        if let Outbound::Delta(Delta::Terminal { terminal: t, data }) = event
            && t.as_str() == terminal
        {
            seen.push_str(&String::from_utf8_lossy(&B64.decode(data).unwrap()));
        }
    }
}

fn terminal(s: &Records, id: &str) -> Option<Terminal> {
    match s.live.get(&(EntityKind::Terminal, id.to_string())) {
        Some(Entity::Terminal(t)) => Some(t.clone()),
        _ => None,
    }
}

fn input(id: &str, text: &str) -> Command {
    Command::TerminalInput { terminal: TerminalId(id.into()), data: B64.encode(text) }
}

#[tokio::test]
async fn a_terminal_echoes_input_keeps_scrollback_resizes_and_exits() {
    let w = World::new(vec![FakeProvider::new("codex")]).await;
    let mut rx = w.host.subscribe();
    let reply = w
        .run(Command::TerminalOpen {
            project: Some(w.project.clone()),
            thread: None,
            command: Some("printf 'ready\\n'; read a; echo \"got:$a\"; stty size; pwd; read b; exit 3".into()),
            script: None,
            cols: 100,
            rows: 30,
        })
        .await;
    let Reply::Created { id } = reply else { panic!("{reply:?}") };
    let t = terminal(&w.state().await, &id).unwrap();
    assert_eq!((t.status, t.cols, t.rows, t.project.as_ref()), (TerminalStatus::Running, 100, 30, Some(&w.project)));
    assert_eq!(t.cwd, w.repo.to_string_lossy());

    let mut seen = String::new();
    read_until(&mut rx, &id, &mut seen, "ready").await;
    assert_eq!(w.run(input(&id, "hello\r")).await, Reply::Ok);
    read_until(&mut rx, &id, &mut seen, "got:hello").await;
    read_until(&mut rx, &id, &mut seen, "30 100").await;
    read_until(&mut rx, &id, &mut seen, &w.repo.to_string_lossy()).await;

    let Reply::Scrollback { data } = w.run(Command::TerminalScrollback { terminal: TerminalId(id.clone()) }).await else {
        panic!()
    };
    let scrollback = String::from_utf8(B64.decode(data).unwrap()).unwrap();
    assert!(scrollback.contains("ready") && scrollback.contains("got:hello"), "{scrollback:?}");

    assert_eq!(w.run(Command::TerminalResize { terminal: TerminalId(id.clone()), cols: 120, rows: 40 }).await, Reply::Ok);
    assert_eq!(terminal(&w.state().await, &id).unwrap().cols, 120);
    let raw = Command::TerminalInput { terminal: TerminalId(id.clone()), data: "not base64!".into() };
    assert!(matches!(w.run(raw).await, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })));

    w.run(input(&id, "bye\r")).await;
    let exited = w
        .wait("the exit", |s| terminal(s, &id).filter(|t| t.status == TerminalStatus::Exited))
        .await;
    assert_eq!(exited.exit_code, Some(3));
    let late = w.run(input(&id, "x")).await;
    assert!(matches!(late, Reply::Error(ReplyError { code: ErrorCode::Conflict, .. })), "{late:?}");
    assert_eq!(w.run(Command::TerminalClose { terminal: TerminalId(id.clone()) }).await, Reply::Ok);
    assert!(terminal(&w.state().await, &id).is_none());
}

#[tokio::test]
async fn closing_hangs_up_a_login_shell() {
    let w = World::new(vec![FakeProvider::new("codex")]).await;
    let mut rx = w.host.subscribe();
    let Reply::Created { id } =
        w.run(Command::TerminalOpen { project: None, thread: None, command: None, script: None, cols: 80, rows: 24 }).await
    else {
        panic!()
    };
    let mut seen = String::new();
    let flag = w.home.join("hung-up");
    w.run(input(&id, &format!("trap 'echo hup > {}; exit' HUP; echo marker-$((40+2))\r", flag.display()))).await;
    read_until(&mut rx, &id, &mut seen, "marker-42").await;
    assert_eq!(w.run(Command::TerminalClose { terminal: TerminalId(id.clone()) }).await, Reply::Ok);
    let s = w.state().await;
    assert!(terminal(&s, &id).is_none());
    assert!(s.replay.iter().any(|st| matches!(&st.entity, Entity::Removed(r) if r.id == id)));
    // The shell got the hang-up, and its exit doesn't bring the record back.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !flag.exists() {
        assert!(tokio::time::Instant::now() < deadline, "the shell never saw SIGHUP");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(terminal(&w.state().await, &id).is_none());
    let reply = w.run(input(&id, "echo again\r")).await;
    assert!(matches!(reply, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })), "{reply:?}");
}

#[tokio::test]
async fn thread_terminals_run_in_the_thread_and_go_with_it() {
    let w = World::new(vec![FakeProvider::new("codex")]).await;
    let thread = w.thread("codex", None, true).await;
    let cwd = w.state().await.thread(&thread).unwrap().cwd.clone();
    let mut rx = w.host.subscribe();
    let Reply::Created { id } = w
        .run(Command::TerminalOpen {
            project: None,
            thread: Some(thread.clone()),
            command: Some("pwd; read x".into()),
            script: None,
            cols: 80,
            rows: 24,
        })
        .await
    else {
        panic!()
    };
    let t = terminal(&w.state().await, &id).unwrap();
    assert_eq!((t.thread.as_ref(), t.project.as_ref()), (Some(&thread), Some(&w.project)));
    let mut seen = String::new();
    read_until(&mut rx, &id, &mut seen, &cwd).await;
    assert_eq!(w.run(Command::ThreadDelete { thread: thread.clone() }).await, Reply::Ok);
    assert!(terminal(&w.state().await, &id).is_none());
    let unknown = w.run(Command::TerminalScrollback { terminal: TerminalId("trm_nope".into()) }).await;
    assert!(matches!(unknown, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));
}

/// A login finished in a terminal shows on the machine without a refresh: when a terminal exits
/// while a provider is logged out, the daemon probes again and says hello with what it found.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_exiting_while_logged_out_checks_the_providers_again() {
    let claude = FakeProvider::new("claude");
    claude.set_auth(AuthStatus::Missing);
    let w = World::new(vec![claude.clone()]).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let p = w.host.info().await.providers[0].clone();
        if p.auth == AuthStatus::Missing && !p.checking {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the first probe never answered: {p:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // "Logging in": the command ends, and the provider is logged in from then on.
    let mut events = w.host.subscribe();
    let open = |command: &str| Command::TerminalOpen { project: None, thread: None, command: Some(command.into()), script: None, cols: 80, rows: 24 };
    let Reply::Created { id } = w.run(open("read done")).await else { panic!("terminal.open") };
    claude.set_auth(AuthStatus::Ready);
    assert_eq!(w.run(input(&id, "ok\r")).await, Reply::Ok);
    let seen = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await.expect("hello with the login").unwrap() {
            Outbound::Info(info) if info.providers[0].auth == AuthStatus::Ready && !info.providers[0].checking => break info.providers[0].clone(),
            _ => {}
        }
    };
    assert!(seen.installed, "{seen:?}");

    // Logged in: another terminal exiting checks nothing.
    let mut events = w.host.subscribe();
    let Reply::Created { id } = w.run(open("true")).await else { panic!("terminal.open") };
    w.wait("the exit", |s| terminal(s, &id).filter(|t| t.status == TerminalStatus::Exited)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let probed = std::iter::from_fn(|| events.try_recv().ok()).any(|o| matches!(o, Outbound::Info(i) if i.providers[0].checking));
    assert!(!probed, "nothing to check once logged in");
}
