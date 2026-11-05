//! Thread commands on fake providers against real git repos: worktrees, provider handoffs, forks,
//! rollback, delete, publish, diffs, files, projects.

mod common;

use agent_host::provider::fake::{Call, FakeProvider, FakeSession, Script};
use agent_host::provider::{ForkAt, ForkFrom, ProviderEvent, RollbackStyle, TurnEnd};
use agent_proto::*;
use common::*;
use std::path::Path;
use std::sync::Arc;

fn manual(id: &str, caps: Capabilities) -> Arc<FakeProvider> {
    FakeProvider::with(id, caps, Script::Manual)
}

fn sends(calls: &[Call]) -> Vec<String> {
    calls.iter().filter_map(|c| if let Call::Send(t) = c { Some(t.clone()) } else { None }).collect()
}

fn is_err(reply: &Reply, code: ErrorCode) -> bool {
    matches!(reply, Reply::Error(ReplyError { code: c, .. }) if *c == code)
}

/// Runs one manual turn that writes `files` into the thread's directory.
async fn turn(w: &World, session: &FakeSession, thread: &ThreadId, files: &[(&str, &str)]) {
    let cwd = w.wait_thread(thread, |t| t.status == ThreadStatus::Running).await.cwd;
    for (name, text) in files {
        std::fs::write(Path::new(&cwd).join(name), text).unwrap();
    }
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(thread, |t| t.status == ThreadStatus::Idle && t.active_run.is_none()).await;
}

#[tokio::test]
async fn worktree_threads_get_a_branch_and_delete_leaves_only_tombstones_and_the_branch() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("Fix the parser"), true).await;
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 1).await;
    let wt = t.worktree.clone().unwrap();
    assert_eq!(wt.branch, "roll/fix-the-parser");
    assert_eq!(t.cwd, wt.path);
    assert_eq!(wt.path, w.home.join(format!("worktrees/{}/{thread}", w.project)).to_string_lossy());
    assert!(Path::new(&wt.path).join("README.md").exists());
    assert_eq!(fake.session(0).await.config.cwd, Path::new(&wt.path));

    assert_eq!(w.run(Command::ThreadDelete { thread: thread.clone() }).await, Reply::Ok);
    assert!(!Path::new(&wt.path).exists());
    assert!(sh(&w.repo, &["branch", "--list", "roll/fix-the-parser"]).contains("roll/fix-the-parser"));
    assert!(sh(&w.repo, &["for-each-ref", "refs/roll/"]).is_empty());
    let s = w.state().await;
    assert!(s.live.values().all(|e| e.thread() != Some(&thread)));
    let removed: Vec<EntityKind> = s
        .replay
        .iter()
        .filter_map(|st| match &st.entity {
            Entity::Removed(r) if r.thread.as_ref() == Some(&thread) => Some(r.entity),
            _ => None,
        })
        .collect();
    for kind in [EntityKind::Thread, EntityKind::Run, EntityKind::Node, EntityKind::Checkpoint] {
        assert!(removed.contains(&kind), "{kind:?} tombstone in {removed:?}");
    }
    assert_eq!(removed.last(), Some(&EntityKind::Thread));
    assert!(fake.session(0).await.calls().contains(&Call::Close));
    assert!(is_err(&w.send(&thread, "hi", SendMode::Queue).await, ErrorCode::NotFound));
}

#[tokio::test]
async fn switching_providers_hands_over_what_the_new_one_has_not_seen() {
    let codex = FakeProvider::new("codex");
    let claude = FakeProvider::new("claude");
    let w = World::new(vec![codex.clone(), claude.clone()]).await;
    let thread = w.thread("codex", Some("first question"), false).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;

    let busy = w
        .run(Command::ThreadConfigure {
            thread: thread.clone(),
            title: None,
            provider: Some(ProviderId("nope".into())),
            model: None,
            effort: None,
            options: Default::default(),
            runtime_mode: None,
            interaction_mode: None,
        })
        .await;
    assert!(is_err(&busy, ErrorCode::Invalid));
    let switch = |p: &str| Command::ThreadConfigure {
        thread: thread.clone(),
        title: None,
        provider: Some(ProviderId(p.into())),
        model: None,
        effort: None,
        options: Default::default(),
        runtime_mode: None,
        interaction_mode: None,
    };
    assert_eq!(w.run(switch("claude")).await, Reply::Ok);
    w.send(&thread, "second question", SendMode::Queue).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 2).await;
    let to_claude = sends(&claude.session(0).await.calls()).remove(0);
    assert!(to_claude.starts_with("<roll-context>"), "{to_claude}");
    assert!(to_claude.contains("first question") && to_claude.contains("echo: first question"));
    assert!(to_claude.ends_with("</roll-context>\n\nsecond question"));
    assert!(codex.session(0).await.calls().contains(&Call::Close), "the old session ends");

    assert_eq!(w.run(switch("codex")).await, Reply::Ok);
    w.send(&thread, "third", SendMode::Queue).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 3).await;
    let back = codex.session(1).await;
    assert_eq!(back.config.resume.as_deref(), Some(codex.session(0).await.native()), "codex resumes its own thread");
    let to_codex = sends(&back.calls()).remove(0);
    assert!(to_codex.contains("second question") && to_codex.contains("run=2]"));
    assert!(!to_codex.contains("run=1]"), "codex already has run 1: {to_codex}");
    let runs = w.state().await.runs(&thread);
    assert_eq!(runs.iter().map(|r| r.provider.0.as_str()).collect::<Vec<_>>(), vec!["codex", "claude", "codex"]);
    assert_eq!(runs[2].prompt, "third", "the run records what the user sent, not the handoff");
}

#[tokio::test]
async fn mode_changes_switch_live_or_restart_with_resume() {
    let switching = manual("codex", Capabilities { mode_switch: true, ..Capabilities::default() });
    let restarting = manual("claude", Capabilities::default());
    let w = World::new(vec![switching.clone(), restarting.clone()]).await;
    for (provider, fake) in [("codex", &switching), ("claude", &restarting)] {
        let thread = w.thread(provider, Some("one"), false).await;
        let first = fake.session(0).await;
        turn(&w, &first, &thread, &[]).await;
        w.run(Command::ThreadConfigure {
            thread: thread.clone(),
            title: None,
            provider: None,
            model: None,
            effort: None,
            options: Default::default(),
            runtime_mode: Some(RuntimeMode::Ask),
            interaction_mode: Some(InteractionMode::Plan),
        })
        .await;
        w.send(&thread, "two", SendMode::Queue).await;
        if provider == "codex" {
            let calls = first.wait_calls(|c| sends(c).len() == 2).await;
            assert!(calls.contains(&Call::SetModes(RuntimeMode::Ask, InteractionMode::Plan)));
            assert_eq!(switching.sessions().len(), 1);
        } else {
            let second = fake.session(1).await;
            assert!(first.calls().contains(&Call::Close));
            assert_eq!(second.config.resume.as_deref(), Some(first.native()));
            assert_eq!((second.config.runtime_mode, second.config.interaction_mode), (RuntimeMode::Ask, InteractionMode::Plan));
            assert_eq!(sends(&second.wait_calls(|c| !c.is_empty()).await), vec!["two"]);
        }
    }
}

#[tokio::test]
async fn forks_carry_a_summary_and_the_files_of_their_run() {
    let fake = manual("codex", Capabilities::default());
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("make one"), false).await;
    let session = fake.session(0).await;
    turn(&w, &session, &thread, &[("one.txt", "1\n")]).await;
    w.send(&thread, "make two", SendMode::Queue).await;
    turn(&w, &session, &thread, &[("two.txt", "2\n")]).await;
    let runs = w.state().await.runs(&thread);

    let Reply::Created { id } = w.run(Command::ThreadFork { thread: thread.clone(), at: Some(runs[0].id.clone()), worktree: true }).await else {
        panic!()
    };
    let fork = ThreadId(id);
    let f = w.state().await.thread(&fork).cloned().unwrap();
    assert_eq!(f.lineage, Some(Lineage {
        parent: thread.clone(),
        relation: Relation::Fork,
        root: thread.clone(),
        from_run: Some(runs[0].id.clone())
    }));
    let cwd = Path::new(&f.cwd);
    assert_eq!(std::fs::read_to_string(cwd.join("one.txt")).unwrap(), "1\n");
    assert!(!cwd.join("two.txt").exists());
    assert_eq!(f.worktree.as_ref().unwrap().base, sh(&w.repo, &["rev-parse", "HEAD"]).trim());
    assert_eq!(f.run_count, 0);
    assert_eq!(fake.sessions().len(), 1, "forking starts nothing");

    w.send(&fork, "continue", SendMode::Queue).await;
    let forked = fake.session(1).await;
    assert_eq!(forked.config.resume, None);
    let text = sends(&forked.wait_calls(|c| !c.is_empty()).await).remove(0);
    assert!(text.contains("make one") && !text.contains("make two"), "{text}");
    assert!(text.ends_with("continue"));

    // A fork of a fork keeps the root.
    let Reply::Created { id } = w.run(Command::ThreadFork { thread: fork.clone(), at: None, worktree: false }).await else { panic!() };
    let grandchild = w.state().await.thread(&ThreadId(id)).cloned().unwrap();
    assert_eq!(grandchild.lineage.as_ref().unwrap().root, thread);
    assert_eq!(grandchild.cwd, f.cwd);
    // Unfinished runs can't be forked.
    let active = w.state().await.thread(&fork).unwrap().active_run.clone().unwrap();
    let refused = w.run(Command::ThreadFork { thread: fork.clone(), at: Some(active), worktree: false }).await;
    assert!(is_err(&refused, ErrorCode::Conflict));
}

/// The `system` notes of a thread's nodes, in order.
fn notes(s: &Records, thread: &ThreadId) -> Vec<String> {
    s.nodes(thread).into_iter().filter_map(|n| if let NodeBody::System { text } = n.body { Some(text) } else { None }).collect()
}

fn forking(id: &str) -> Arc<FakeProvider> {
    FakeProvider::with(id, Capabilities { fork: true, ..Capabilities::default() }, Script::Echo)
}

async fn fork(w: &World, thread: &ThreadId, at: Option<RunId>) -> ThreadId {
    let reply = w.run(Command::ThreadFork { thread: thread.clone(), at, worktree: false }).await;
    let Reply::Created { id } = reply else { panic!("thread.fork: {reply:?}") };
    ThreadId(id)
}

/// Sends a message and waits for its run to finish.
async fn exchange(w: &World, thread: &ThreadId, text: &str) {
    let before = w.state().await.thread(thread).unwrap().run_count;
    w.send(thread, text, SendMode::Queue).await;
    w.wait_thread(thread, |t| t.run_count == before + 1 && t.status == ThreadStatus::Idle).await;
}

#[tokio::test]
async fn forks_fork_the_native_conversation_when_the_provider_can() {
    let codex = forking("codex");
    let w = World::new(vec![codex.clone()]).await;
    let thread = w.thread("codex", None, false).await;
    exchange(&w, &thread, "one").await;
    exchange(&w, &thread, "two").await;
    let source = codex.session(0).await;
    let runs = w.state().await.runs(&thread);

    // Forking still starts nothing: the first message on the fork settles it.
    let at_one = fork(&w, &thread, Some(runs[0].id.clone())).await;
    assert_eq!(codex.sessions().len(), 1);
    exchange(&w, &at_one, "three").await;
    let forked = codex.session(1).await;
    assert_eq!(forked.config.fork, Some(ForkFrom { thread: source.native().into(), at: ForkAt::Turn("codex-native-0:turn-1".into()) }));
    assert_eq!(forked.config.resume, None);
    assert_eq!(sends(&forked.calls()), vec!["three"], "the copy holds run 1: nothing is handed over");
    let s = w.state().await;
    assert_eq!(notes(&s, &at_one), vec!["Forked natively: codex goes on from its own conversation in the parent thread"]);
    assert_eq!(s.nodes(&at_one)[0].run, s.runs(&at_one)[0].id, "noted in the run that settled it");

    // The fork goes on in its copy; the parent in its own conversation.
    exchange(&w, &at_one, "four").await;
    assert_eq!(sends(&forked.calls()), vec!["three", "four"]);
    exchange(&w, &thread, "five").await;
    assert_eq!(sends(&source.calls()), vec!["one", "two", "five"]);
    assert_eq!(codex.sessions().len(), 2);

    // The latest run, the default fork point.
    let latest = fork(&w, &thread, None).await;
    exchange(&w, &latest, "six").await;
    assert_eq!(codex.session(2).await.config.fork.unwrap().at, ForkAt::Turn("codex-native-0:turn-3".into()));
    assert_eq!(notes(&w.state().await, &latest).len(), 1);
}

#[tokio::test]
async fn forks_hand_a_summary_over_when_a_native_fork_cant_be_had() {
    let codex = forking("codex");
    let claude = FakeProvider::new("claude");
    let w = World::new(vec![codex.clone(), claude.clone()]).await;
    let thread = w.thread("codex", None, false).await;
    exchange(&w, &thread, "one").await;
    let summary = "Forked with a summary: codex starts fresh with a summary of the parent thread";

    // The provider refuses the fork: the same run starts fresh with the summary.
    let refused = fork(&w, &thread, None).await;
    codex.fail_next_start("no such thread");
    exchange(&w, &refused, "two").await;
    let fresh = codex.session(1).await;
    assert_eq!((fresh.config.fork.clone(), fresh.config.resume.clone()), (None, None));
    let text = sends(&fresh.calls()).remove(0);
    assert!(text.starts_with("<roll-context>") && text.contains("one") && text.ends_with("two"), "{text}");
    assert_eq!(notes(&w.state().await, &refused), vec![summary]);
    assert_eq!(w.state().await.runs(&refused)[0].status, RunStatus::Completed);
    // Settled once: later sessions resume the fork's own conversation.
    exchange(&w, &refused, "three").await;
    assert_eq!(sends(&fresh.calls()), vec![text, "three".to_string()]);
    assert_eq!(notes(&w.state().await, &refused).len(), 1);

    // Another provider runs the fork's first message: it can't continue codex's conversation.
    let switched = fork(&w, &thread, None).await;
    let to_claude = Command::ThreadConfigure {
        thread: switched.clone(),
        title: None,
        provider: Some(ProviderId("claude".into())),
        model: None,
        effort: None,
        options: Default::default(),
        runtime_mode: None,
        interaction_mode: None,
    };
    assert_eq!(w.run(to_claude).await, Reply::Ok);
    exchange(&w, &switched, "four").await;
    assert!(sends(&claude.session(0).await.calls())[0].contains("<roll-context>"));
    assert_eq!(notes(&w.state().await, &switched), vec!["Forked with a summary: claude starts fresh with a summary of the parent thread"]);

    // A provider without native forks.
    let plain = FakeProvider::new("codex");
    let w = World::new(vec![plain.clone()]).await;
    let thread = w.thread("codex", None, false).await;
    exchange(&w, &thread, "one").await;
    let portable = fork(&w, &thread, None).await;
    exchange(&w, &portable, "two").await;
    assert_eq!(plain.session(1).await.config.fork, None);
    assert_eq!(notes(&w.state().await, &portable), vec![summary]);
}

#[tokio::test]
async fn forks_at_a_turn_work_after_a_restart() {
    let codex = forking("codex");
    let w = World::new(vec![codex.clone()]).await;
    let thread = w.thread("codex", None, false).await;
    exchange(&w, &thread, "one").await;
    exchange(&w, &thread, "two").await;
    let native = codex.session(0).await.native().to_string();
    let runs = w.state().await.runs(&thread);
    // A fork made before the restart and used after it.
    let early = fork(&w, &thread, Some(runs[1].id.clone())).await;
    let home = w.home.clone();
    let World { host, dirs, .. } = w;
    drop(host);

    let codex = forking("codex");
    let host = open(&home, vec![codex.clone()]).await;
    let send = |thread: &ThreadId, text: &str| Command::MessageSend {
        thread: thread.clone(),
        text: text.into(),
        attachments: Vec::new(),
        mode: SendMode::Queue,
    };
    let Reply::Created { id } = run(&host, Command::ThreadFork { thread: thread.clone(), at: Some(runs[0].id.clone()), worktree: false }).await else {
        panic!()
    };
    let late = ThreadId(id);
    assert!(matches!(run(&host, send(&late, "three")).await, Reply::Created { .. }));
    let forked = codex.session(0).await;
    // The turn refs were kept with the parent's state: run 1's turn is still known.
    assert_eq!(forked.config.fork, Some(ForkFrom { thread: native.clone(), at: ForkAt::Turn(format!("{native}:turn-1")) }));
    assert_eq!(sends(&forked.wait_calls(|c| !c.is_empty()).await), vec!["three"]);

    assert!(matches!(run(&host, send(&early, "four")).await, Reply::Created { .. }));
    let forked = codex.session(1).await;
    assert_eq!(forked.config.fork, Some(ForkFrom { thread: native.clone(), at: ForkAt::Turn(format!("{native}:turn-2")) }));
    assert_eq!(sends(&forked.wait_calls(|c| !c.is_empty()).await), vec!["four"]);
    drop(dirs);
}

#[tokio::test]
async fn rollback_restores_files_and_starts_the_provider_fresh_with_a_summary() {
    let fake = manual("codex", Capabilities::default());
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("add a"), false).await;
    let session = fake.session(0).await;
    turn(&w, &session, &thread, &[("a.txt", "a\n")]).await;
    w.send(&thread, "add b", SendMode::Queue).await;
    turn(&w, &session, &thread, &[("b.txt", "b\n"), ("a.txt", "a changed\n")]).await;
    let s = w.state().await;
    let checkpoints = s.checkpoints(&thread);
    assert_eq!(checkpoints.iter().map(|c| c.ordinal).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert_eq!(checkpoints[2].stat, DiffStat { files: 2, additions: 2, deletions: 1 });

    let reply = w.run(Command::CheckpointRollback { thread: thread.clone(), checkpoint: checkpoints[1].id.clone() }).await;
    assert_eq!(reply, Reply::Ok);
    assert_eq!(std::fs::read_to_string(w.repo.join("a.txt")).unwrap(), "a\n");
    assert!(!w.repo.join("b.txt").exists());
    let s = w.state().await;
    let runs = s.runs(&thread);
    assert_eq!(runs.iter().map(|r| r.status).collect::<Vec<_>>(), vec![RunStatus::Completed, RunStatus::RolledBack]);
    assert_eq!(s.checkpoints(&thread).len(), 2);
    assert!(sh(&w.repo, &["for-each-ref", &format!("refs/roll/checkpoints/{thread}/2")]).is_empty());
    assert!(session.calls().contains(&Call::Close), "no native rollback: the session ends");

    w.send(&thread, "add c", SendMode::Queue).await;
    let fresh = fake.session(1).await;
    assert_eq!(fresh.config.resume, None);
    let text = sends(&fresh.wait_calls(|c| !c.is_empty()).await).remove(0);
    assert!(text.contains("add a") && !text.contains("add b"), "{text}");
    turn(&w, &fresh, &thread, &[("c.txt", "c\n")]).await;
    // Run 3's diff is against the checkpoint it rolled back to.
    let run3 = w.state().await.runs(&thread).pop().unwrap();
    let Reply::Diff(diff) = w.run(Command::DiffGet { thread: thread.clone(), run: Some(run3.id) }).await else { panic!() };
    assert_eq!(diff.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["c.txt"]);
}

#[tokio::test]
async fn rollback_uses_the_providers_own_rollback_when_it_has_one() {
    let fake = manual("codex", Capabilities { rollback: true, ..Capabilities::default() });
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    turn(&w, &session, &thread, &[]).await;
    w.send(&thread, "two", SendMode::Queue).await;
    turn(&w, &session, &thread, &[]).await;
    w.send(&thread, "three", SendMode::Queue).await;
    turn(&w, &session, &thread, &[]).await;
    let cp = w.state().await.checkpoints(&thread)[1].clone();
    assert_eq!(w.run(Command::CheckpointRollback { thread: thread.clone(), checkpoint: cp.id }).await, Reply::Ok);
    assert!(session.calls().contains(&Call::Rollback(2)));
    w.send(&thread, "four", SendMode::Queue).await;
    let calls = session.wait_calls(|c| sends(c).len() == 4).await;
    assert_eq!(sends(&calls)[3], "four", "same session, nothing handed over");
    assert_eq!(fake.sessions().len(), 1);
}

/// A provider that rolls back by resuming at a kept turn, as Claude does.
fn resuming(id: &str) -> Arc<FakeProvider> {
    let fake = FakeProvider::with(id, Capabilities { rollback: true, fork: true, ..Capabilities::default() }, Script::Echo);
    fake.set_rollback_style(RollbackStyle::ResumeAt);
    fake
}

/// The world again, on a new daemon process over the same home (the old one is dropped).
async fn restart(w: World, providers: Vec<Arc<FakeProvider>>) -> World {
    let World { host, home, repo, project, dirs } = w;
    drop(host);
    World { host: open(&home, providers).await, home, repo, project, dirs }
}

#[tokio::test]
async fn rollback_resumes_at_the_kept_turn_after_a_restart() {
    let claude = resuming("claude");
    let w = World::new(vec![claude.clone()]).await;
    let thread = w.thread("claude", None, false).await;
    for text in ["one", "two", "three"] {
        exchange(&w, &thread, text).await;
    }
    let native = claude.session(0).await.native().to_string();

    // A new daemon: no live session, and nothing in memory about the conversation.
    let claude = resuming("claude");
    let w = restart(w, vec![claude.clone()]).await;
    let cp = w.state().await.checkpoints(&thread)[1].clone();
    assert_eq!(w.run(Command::CheckpointRollback { thread: thread.clone(), checkpoint: cp.id }).await, Reply::Ok);
    assert!(claude.sessions().is_empty(), "nothing to do in a session: the next one resumes at the turn");
    exchange(&w, &thread, "four").await;
    let resumed = claude.session(0).await;
    assert_eq!((resumed.config.resume.as_deref(), resumed.config.resume_at.as_deref()), (Some(native.as_str()), Some(format!("{native}:turn-1").as_str())));
    assert_eq!(sends(&resumed.calls()), vec!["four"], "the conversation holds run 1: nothing is handed over");

    // Sessions keep resuming at the kept turn until one goes past it, across restarts too.
    let claude = resuming("claude");
    let w = restart(w, vec![claude.clone()]).await;
    exchange(&w, &thread, "five").await;
    let s = claude.session(0).await;
    assert_eq!((s.config.resume.as_deref(), s.config.resume_at.as_deref()), (Some(native.as_str()), None), "run 4 went past it");
    assert_eq!(sends(&s.calls()), vec!["five"]);
}

#[tokio::test]
async fn rollback_without_a_kept_turn_ref_hands_the_later_runs_over() {
    let claude = resuming("claude");
    let codex = FakeProvider::new("codex");
    let w = World::new(vec![claude.clone(), codex.clone()]).await;
    let thread = w.thread("claude", None, false).await;
    exchange(&w, &thread, "one").await;
    let switch = |provider: &str| Command::ThreadConfigure {
        thread: thread.clone(),
        title: None,
        provider: Some(ProviderId(provider.into())),
        model: None,
        effort: None,
        options: Default::default(),
        runtime_mode: None,
        interaction_mode: None,
    };
    // Run 2 on codex, run 3 back on claude (with run 2 handed over in its prompt).
    assert_eq!(w.run(switch("codex")).await, Reply::Ok);
    exchange(&w, &thread, "two").await;
    assert_eq!(w.run(switch("claude")).await, Reply::Ok);
    exchange(&w, &thread, "three").await;
    let native = claude.session(0).await.native().to_string();

    // Back to run 2: claude's conversation goes on from its run 1, and gets run 2 handed over.
    let cp = w.state().await.checkpoints(&thread)[2].clone();
    assert_eq!(w.run(Command::CheckpointRollback { thread: thread.clone(), checkpoint: cp.id }).await, Reply::Ok);
    exchange(&w, &thread, "four").await;
    // Session 1 ran run 3; session 2 is the one after the rollback.
    let s = claude.session(2).await;
    assert_eq!(s.config.resume_at, Some(format!("{native}:turn-1")));
    let text = sends(&s.calls()).remove(0);
    assert!(text.starts_with("<roll-context>") && text.contains("two") && !text.contains("three") && text.ends_with("four"), "{text}");
}

#[tokio::test]
async fn rolling_a_native_fork_back_before_its_runs_copies_the_parent_again() {
    let claude = resuming("claude");
    let w = World::new(vec![claude.clone()]).await;
    let thread = w.thread("claude", None, false).await;
    exchange(&w, &thread, "one").await;
    exchange(&w, &thread, "two").await;
    let parent = claude.session(0).await.native().to_string();
    let runs = w.state().await.runs(&thread);
    let forked = fork(&w, &thread, Some(runs[0].id.clone())).await;
    exchange(&w, &forked, "three").await;
    let at = ForkFrom { thread: parent.clone(), at: ForkAt::Turn(format!("{parent}:turn-1")) };
    assert_eq!(claude.session(1).await.config.fork, Some(at.clone()));

    // Back to before the fork's own first run, after a restart: a new copy at the fork point.
    let claude = resuming("claude");
    let w = restart(w, vec![claude.clone()]).await;
    let base = w.state().await.checkpoints(&forked)[0].clone();
    assert_eq!(base.ordinal, 0);
    assert_eq!(w.run(Command::CheckpointRollback { thread: forked.clone(), checkpoint: base.id }).await, Reply::Ok);
    exchange(&w, &forked, "four").await;
    let copy = claude.session(0).await;
    assert_eq!((copy.config.fork.clone(), copy.config.resume.clone()), (Some(at), None));
    assert_eq!(sends(&copy.calls()), vec!["four"], "the copy holds the parent through run 1");
    // The new copy is the fork's conversation from now on.
    exchange(&w, &forked, "five").await;
    assert_eq!(sends(&copy.calls()), vec!["four", "five"]);

    // If the parent's conversation can't be copied, the fork starts fresh with its summary.
    let cp = w.state().await.checkpoints(&forked)[0].clone();
    assert_eq!(w.run(Command::CheckpointRollback { thread: forked.clone(), checkpoint: cp.id }).await, Reply::Ok);
    claude.fail_next_start("no such session");
    exchange(&w, &forked, "six").await;
    let fresh = claude.session(1).await;
    assert_eq!((fresh.config.fork.clone(), fresh.config.resume.clone()), (None, None));
    let text = sends(&fresh.calls()).remove(0);
    assert!(text.starts_with("<roll-context>") && text.contains("one") && !text.contains("two") && text.ends_with("six"), "{text}");
}

#[tokio::test]
async fn busy_threads_refuse_rollback_publish_archive_and_provider_switches() {
    let fake = manual("codex", Capabilities::default());
    let other = FakeProvider::new("claude");
    let w = World::new(vec![fake.clone(), other]).await;
    let thread = w.thread("codex", Some("go"), true).await;
    fake.session(0).await.wait_calls(|c| !c.is_empty()).await;
    let cp = w.wait("the base checkpoint", |s| s.checkpoints(&thread).first().cloned()).await;
    for command in [
        Command::CheckpointRollback { thread: thread.clone(), checkpoint: cp.id.clone() },
        Command::ThreadPublish { thread: thread.clone(), message: "x".into(), land: false },
        Command::ThreadArchive { thread: thread.clone() },
        Command::ThreadConfigure {
            thread: thread.clone(),
            title: None,
            provider: Some(ProviderId::claude()),
            model: None,
            effort: None,
            options: Default::default(),
            runtime_mode: None,
            interaction_mode: None,
        },
    ] {
        let reply = w.run(command.clone()).await;
        assert!(is_err(&reply, ErrorCode::Conflict), "{command:?}: {reply:?}");
    }
}

#[tokio::test]
async fn publish_commits_the_worktree_and_pushes_its_branch() {
    let fake = manual("codex", Capabilities::default());
    let w = World::new(vec![fake.clone()]).await;
    let remote = tempfile::tempdir().unwrap();
    sh(remote.path(), &["init", "-q", "--bare"]);
    sh(&w.repo, &["remote", "add", "origin", remote.path().to_str().unwrap()]);
    let thread = w.thread("codex", Some("Add docs"), true).await;
    turn(&w, &fake.session(0).await, &thread, &[("docs.md", "# docs\n")]).await;
    let reply = w.run(Command::ThreadPublish { thread: thread.clone(), message: "Add docs".into(), land: false }).await;
    let Reply::Published(published) = reply else { panic!("{reply:?}") };
    assert_eq!(published.branch, "roll/add-docs");
    assert_eq!(sh(remote.path(), &["rev-parse", "refs/heads/roll/add-docs"]).trim(), published.commit);
    assert_eq!((published.change, published.url), (None, None));
    let Reply::GitStatus(status) = w.run(Command::GitStatus { thread: thread.clone() }).await else { panic!() };
    assert_eq!(status.branch.as_deref(), Some("roll/add-docs"));
    assert!(status.files.is_empty());

    let plain = w.thread("codex", None, false).await;
    let refused = w.run(Command::ThreadPublish { thread: plain, message: "x".into(), land: false }).await;
    assert!(is_err(&refused, ErrorCode::Invalid));
    // Only a roll code repo lands.
    let land = w.run(Command::ThreadPublish { thread: thread.clone(), message: "x".into(), land: true }).await;
    assert!(is_err(&land, ErrorCode::Invalid), "{land:?}");
}

#[tokio::test]
async fn diffs_of_a_run_and_of_the_whole_thread() {
    let fake = manual("codex", Capabilities::default());
    let w = World::new(vec![fake.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = fake.session(0).await;
    turn(&w, &session, &thread, &[("one.txt", "1\n")]).await;
    w.send(&thread, "two", SendMode::Queue).await;
    session.wait_calls(|c| sends(c).len() == 2).await;
    std::fs::write(w.repo.join("two.txt"), "2\n").unwrap();
    let running = w.state().await.thread(&thread).unwrap().active_run.clone().unwrap();
    // A running run's diff is live.
    let Reply::Diff(live) = w.run(Command::DiffGet { thread: thread.clone(), run: Some(running) }).await else { panic!() };
    assert_eq!(live.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["two.txt"]);
    let Reply::Diff(all) = w.run(Command::DiffGet { thread: thread.clone(), run: None }).await else { panic!() };
    assert_eq!(all.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["one.txt", "two.txt"]);
    assert_eq!(all.stat.additions, 2);
    let first = w.state().await.runs(&thread)[0].clone();
    assert_eq!(first.stat, Some(DiffStat { files: 1, additions: 1, deletions: 0 }));
    let Reply::Diff(one) = w.run(Command::DiffGet { thread: thread.clone(), run: Some(first.id) }).await else { panic!() };
    assert_eq!(one.files[0].change, ChangeKind::Add);
    assert!(one.files[0].diff.as_ref().unwrap().contains("+1"));
}

#[tokio::test]
async fn non_git_projects_run_without_checkpoints_or_diffs() {
    let fake = FakeProvider::new("codex");
    let w = World::with_repo(vec![fake.clone()], false).await;
    let thread = w.thread("codex", Some("hi"), false).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 1).await;
    let s = w.state().await;
    assert!(s.checkpoints(&thread).is_empty());
    assert_eq!(s.runs(&thread)[0].status, RunStatus::Completed);
    assert!(is_err(&w.run(Command::DiffGet { thread: thread.clone(), run: None }).await, ErrorCode::Invalid));
    assert!(is_err(&w.run(Command::GitStatus { thread: thread.clone() }).await, ErrorCode::Invalid));
    let worktree = w
        .run(Command::ThreadCreate {
            project: w.project.clone(),
            provider: ProviderId::codex(),
            model: None,
            effort: None,
            options: Default::default(),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            worktree: true,
            title: None,
            prompt: None,
            attachments: Vec::new(),
            orchestrator: false,
        })
        .await;
    assert!(is_err(&worktree, ErrorCode::Invalid));
}

#[tokio::test]
async fn files_are_read_inside_the_project_only() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake]).await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "no").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), w.repo.join("link")).unwrap();
    std::fs::create_dir(w.repo.join("src")).unwrap();
    std::fs::write(w.repo.join("src/lib.rs"), "fn main() {}\n").unwrap();
    std::fs::write(w.repo.join("blob.bin"), [0u8, 1, 2]).unwrap();
    let read = |path: &str| Command::FsRead { project: w.project.clone(), path: path.into() };

    let Reply::File(file) = w.run(read("src/lib.rs")).await else { panic!() };
    assert_eq!((file.text.as_deref(), file.size), (Some("fn main() {}\n"), 13));
    let Reply::File(bin) = w.run(read("/blob.bin")).await else { panic!() };
    assert_eq!((bin.text, bin.size), (None, 3));
    for escape in ["../secret", "src/../../x", "link", "/../etc/passwd"] {
        let reply = w.run(read(escape)).await;
        assert!(is_err(&reply, ErrorCode::Invalid) || is_err(&reply, ErrorCode::NotFound), "{escape}: {reply:?}");
        assert!(!matches!(reply, Reply::File(_)));
    }
    assert!(is_err(&w.run(read("link")).await, ErrorCode::Invalid));
    assert!(is_err(&w.run(read("missing.txt")).await, ErrorCode::NotFound));

    let Reply::Entries { entries } = w.run(Command::FsList { project: Some(w.project.clone()), path: "".into() }).await else { panic!() };
    let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
    assert_eq!(&names[..2], &[(".git", true), ("src", true)]);
    assert!(names.contains(&("README.md", false)));
    let up = w.run(Command::FsList { project: Some(w.project.clone()), path: "..".into() }).await;
    assert!(is_err(&up, ErrorCode::Invalid));
    let Reply::Entries { entries } = w
        .run(Command::FsList { project: None, path: outside.path().to_string_lossy().into() })
        .await
    else {
        panic!()
    };
    assert_eq!(entries[0].name, "secret");
    assert!(is_err(&w.run(Command::FsList { project: None, path: "relative".into() }).await, ErrorCode::Invalid));
}

#[tokio::test]
async fn projects_are_unique_by_path_and_removal_deletes_their_threads() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake]).await;
    let again = w.run(Command::ProjectAdd { path: w.repo.to_string_lossy().into(), name: None, clone: None }).await;
    assert_eq!(again, Reply::Created { id: w.project.0.clone() });
    let s = w.state().await;
    let Some(Entity::Project(p)) = s.live.get(&(EntityKind::Project, w.project.0.clone())) else { panic!() };
    assert_eq!(p.git.as_ref().unwrap().branch.as_deref(), Some("main"));
    let clone = w
        .run(Command::ProjectAdd {
            path: "/tmp/x".into(),
            name: None,
            clone: Some(RepoRef { owner: "a".into(), repo: "b".into() }),
        })
        .await;
    assert!(is_err(&clone, ErrorCode::Invalid));
    assert!(is_err(&w.run(Command::ProjectAdd { path: "/no/such/dir".into(), name: None, clone: None }).await, ErrorCode::Invalid));

    let thread = w.thread("codex", Some("hi"), true).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    assert_eq!(w.run(Command::ProjectRemove { project: w.project.clone() }).await, Reply::Ok);
    let s = w.state().await;
    assert!(s.live.is_empty(), "{:?}", s.live.keys().collect::<Vec<_>>());
    assert!(!w.home.join("worktrees").join(w.project.as_str()).exists());
}

/// A clone that fails on the way (here the repository isn't there yet) keeps no receipt: the
/// same command id runs again and clones. A refusal (the path is taken) is the id's answer.
#[tokio::test]
async fn a_failed_clone_runs_again_with_the_same_id_and_a_refusal_does_not() {
    use agent_host::Daemon;
    let w = World::new(vec![FakeProvider::new("codex")]).await;
    let src = tempfile::tempdir().unwrap();
    let url = src.path().join("app.git");
    let dest = tempfile::tempdir().unwrap();
    let target = dest.path().join("app");
    let repo = RepoRef { owner: "acme".into(), repo: "app".into() };
    let grant = || CodeGrant { repo: repo.clone(), url: url.to_string_lossy().into(), token: "rlg_t".into(), write: false, expires_at: u64::MAX };
    let add = Command::ProjectAdd { path: target.to_string_lossy().into(), name: None, clone: Some(repo.clone()) };

    let first = w.host.run_granted("clone-1".into(), "owner".into(), add.clone(), Some(grant())).await;
    assert!(is_err(&first, ErrorCode::Internal), "{first:?}");
    assert!(!target.exists());
    git_repo(src.path());
    sh(src.path(), &["clone", "-q", "--bare", ".", "app.git"]);
    let again = w.host.run_granted("clone-1".into(), "owner".into(), add, Some(grant())).await;
    assert!(matches!(again, Reply::Created { .. }), "{again:?}");
    assert!(target.join("README.md").exists());

    // A refusal is the id's answer for good.
    let taken = dest.path().join("taken");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("mine.txt"), "x").unwrap();
    let add = Command::ProjectAdd { path: taken.to_string_lossy().into(), name: None, clone: Some(repo.clone()) };
    let refused = w.host.run_granted("clone-2".into(), "owner".into(), add.clone(), Some(grant())).await;
    assert!(is_err(&refused, ErrorCode::Invalid), "{refused:?}");
    std::fs::remove_file(taken.join("mine.txt")).unwrap();
    assert_eq!(w.host.run_granted("clone-2".into(), "owner".into(), add, Some(grant())).await, refused);
}

#[tokio::test]
async fn archive_and_unarchive() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake]).await;
    let thread = w.thread("codex", None, false).await;
    assert_eq!(w.state().await.thread(&thread).unwrap().title, "New thread");
    assert_eq!(w.run(Command::ThreadArchive { thread: thread.clone() }).await, Reply::Ok);
    assert!(w.state().await.thread(&thread).unwrap().archived_at.is_some());
    assert_eq!(w.run(Command::ThreadUnarchive { thread: thread.clone() }).await, Reply::Ok);
    assert!(w.state().await.thread(&thread).unwrap().archived_at.is_none());
    // The first message names an untitled thread.
    w.send(&thread, "Name me after this", SendMode::Queue).await;
    w.wait_thread(&thread, |t| t.title == "Name me after this").await;
}

#[tokio::test]
async fn the_context_window_follows_the_conversation() {
    let codex = manual("codex", Capabilities { rollback: true, ..Capabilities::default() });
    let claude = FakeProvider::new("claude");
    let w = World::new(vec![codex.clone(), claude.clone()]).await;
    let ctx = |used| ContextUsage { used_tokens: used, max_tokens: Some(258_400), total_processed_tokens: Some(used * 2) };
    let thread = w.thread("codex", Some("one"), false).await;
    let session = codex.session(0).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    session.emit(ProviderEvent::Context(ctx(20_000)));
    turn(&w, &session, &thread, &[]).await;
    w.send(&thread, "two", SendMode::Queue).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    session.emit(ProviderEvent::Context(ctx(60_000)));
    turn(&w, &session, &thread, &[]).await;
    let s = w.state().await;
    assert_eq!(s.thread(&thread).unwrap().context, Some(ctx(60_000)));
    let runs = s.runs(&thread);
    assert_eq!(runs.iter().map(|r| r.context).collect::<Vec<_>>(), vec![Some(ctx(20_000)), Some(ctx(60_000))]);

    // A report between runs (an ACP session's) goes on the thread alone.
    session.emit(ProviderEvent::Context(ctx(61_000)));
    w.wait_thread(&thread, |t| t.context == Some(ctx(61_000))).await;
    assert_eq!(w.state().await.runs(&thread)[1].context, Some(ctx(60_000)));

    // A fork shows its parent's context at the fork point until its own conversation exists.
    let forked = fork(&w, &thread, Some(runs[0].id.clone())).await;
    assert_eq!(w.state().await.thread(&forked).unwrap().context, Some(ctx(20_000)));

    // Rolled back natively: the conversation is what it was after run 1.
    let cp = w.state().await.checkpoints(&thread)[1].clone();
    assert_eq!(w.run(Command::CheckpointRollback { thread: thread.clone(), checkpoint: cp.id }).await, Reply::Ok);
    assert!(session.calls().contains(&Call::Rollback(1)));
    assert_eq!(w.state().await.thread(&thread).unwrap().context, Some(ctx(20_000)));

    // Another provider's conversation hasn't reported; switching back shows codex's again.
    let switch = |p: &str| Command::ThreadConfigure {
        thread: thread.clone(),
        title: None,
        provider: Some(ProviderId(p.into())),
        model: None,
        effort: None,
        options: Default::default(),
        runtime_mode: None,
        interaction_mode: None,
    };
    assert_eq!(w.run(switch("claude")).await, Reply::Ok);
    assert_eq!(w.state().await.thread(&thread).unwrap().context, None);
    assert_eq!(w.run(switch("codex")).await, Reply::Ok);
    assert_eq!(w.state().await.thread(&thread).unwrap().context, Some(ctx(20_000)));

    // This codex can't fork natively: the fork starts fresh with a summary, and no context.
    w.send(&forked, "three", SendMode::Queue).await;
    let fork_session = codex.session(1).await;
    turn(&w, &fork_session, &forked, &[]).await;
    assert_eq!(w.state().await.thread(&forked).unwrap().context, None);
}

const DAY: Millis = 86_400_000;

fn now_ms() -> Millis {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn approval(command: &str) -> RequestBody {
    RequestBody::Approval(Approval {
        action: ApprovalAction::Command,
        title: format!("Run `{command}`"),
        command: Some(command.into()),
        cwd: None,
        files: Vec::new(),
        reason: None,
        tool: None,
        input: serde_json::Value::Null,
        decisions: vec![Decision::Accept, Decision::Decline],
    })
}

#[tokio::test]
async fn settling_by_hand_and_what_brings_a_thread_back() {
    let codex = manual("codex", Capabilities::default());
    let w = World::new(vec![codex.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = codex.session(0).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    let settle = Command::ThreadSettle { thread: thread.clone() };
    assert!(is_err(&w.run(settle.clone()).await, ErrorCode::Conflict), "a running thread doesn't settle");
    session.emit(ProviderEvent::Request { id: "rpc-1".into(), item: None, body: approval("ls") });
    w.wait_thread(&thread, |t| t.pending_requests == 1).await;
    assert!(is_err(&w.run(settle.clone()).await, ErrorCode::Conflict), "nor one waiting on its user");
    w.send(&thread, "and then this", SendMode::Queue).await;
    answer(&w, &thread).await;
    assert!(is_err(&w.run(settle.clone()).await, ErrorCode::Conflict), "nor one with a queued message");
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Running && t.run_count == 2).await;
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    let before = w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;

    // Opening it isn't activity.
    assert_eq!(w.run(Command::ThreadVisit { thread: thread.clone() }).await, Reply::Ok);
    let visited = w.state().await.thread(&thread).unwrap().clone();
    assert!(visited.inbox.visited_at.is_some_and(|v| v >= before.updated_at));
    assert_eq!(visited.updated_at, before.updated_at);

    assert_eq!(w.run(settle.clone()).await, Reply::Ok);
    let t = w.state().await.thread(&thread).unwrap().clone();
    let Some(Settle::Settled { at, by: SettledBy::User }) = t.inbox.settle else { panic!("{:?}", t.inbox) };
    assert!(session.calls().contains(&Call::Close), "a settled thread lets go of its session");
    assert_eq!(w.run(settle.clone()).await, Reply::Ok);
    let again = w.state().await.thread(&thread).unwrap().inbox.settle;
    assert_eq!(again, Some(Settle::Settled { at, by: SettledBy::User }), "settling again keeps when");

    // Un-settled by hand: kept active, so the sweep leaves it alone.
    assert_eq!(w.run(Command::ThreadUnsettle { thread: thread.clone() }).await, Reply::Ok);
    assert!(matches!(w.state().await.thread(&thread).unwrap().inbox.settle, Some(Settle::Active { .. })));
    w.host.settle_round(now_ms() + 30 * DAY).await;
    assert!(matches!(w.state().await.thread(&thread).unwrap().inbox.settle, Some(Settle::Active { .. })));

    // A message brings a settled thread back, and spends "keep it active".
    assert_eq!(w.run(settle).await, Reply::Ok);
    w.send(&thread, "one more thing", SendMode::Queue).await;
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    assert_eq!(t.inbox.settle, None);
    assert_eq!(codex.sessions().len(), 2, "a new session picks the conversation up");
}

/// Accepts the thread's pending request.
async fn answer(w: &World, thread: &ThreadId) {
    let request = w.state().await.requests(thread).into_iter().find(|r| r.status == RequestStatus::Pending).unwrap();
    let respond = Command::RequestRespond { request: request.id, response: Response::Decision { decision: Decision::Accept, message: None } };
    assert_eq!(w.run(respond).await, Reply::Ok);
    w.wait_thread(thread, |t| t.pending_requests == 0).await;
}

#[tokio::test]
async fn snoozes_wake_on_time_or_early() {
    let codex = manual("codex", Capabilities::default());
    let w = World::new(vec![codex.clone()]).await;
    let thread = w.thread("codex", Some("one"), false).await;
    let session = codex.session(0).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle).await;
    let snooze = |until| Command::ThreadSnooze { thread: thread.clone(), until };
    assert!(is_err(&w.run(snooze(now_ms() - 1)).await, ErrorCode::Invalid), "the past");
    assert!(is_err(&w.run(snooze(now_ms() + 400 * DAY)).await, ErrorCode::Invalid), "more than a year");

    // Settled, then snoozed: it wakes into the inbox.
    assert_eq!(w.run(Command::ThreadSettle { thread: thread.clone() }).await, Reply::Ok);
    let until = now_ms() + 3_600_000;
    assert_eq!(w.run(snooze(until)).await, Reply::Ok);
    let t = w.state().await.thread(&thread).unwrap().clone();
    assert_eq!(t.inbox.settle, None);
    assert_eq!(t.inbox.snooze.map(|s| s.until), Some(until));
    w.host.settle_round(until - 1).await;
    assert!(w.state().await.thread(&thread).unwrap().inbox.snooze.is_some(), "not yet");
    w.host.settle_round(until).await;
    let t = w.state().await.thread(&thread).unwrap().clone();
    assert_eq!((t.inbox.snooze, t.inbox.woke_at), (None, Some(until)));

    // Woken by hand: no "woke".
    assert_eq!(w.run(snooze(now_ms() + DAY)).await, Reply::Ok);
    assert_eq!(w.run(Command::ThreadUnsnooze { thread: thread.clone() }).await, Reply::Ok);
    let t = w.state().await.thread(&thread).unwrap().clone();
    assert_eq!((t.inbox.snooze, t.inbox.woke_at), (None, Some(until)));

    // A request wakes a snoozed run early, and nothing waiting on its user snoozes.
    w.send(&thread, "two", SendMode::Queue).await;
    w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await;
    let session = codex.sessions().pop().unwrap();
    assert_eq!(w.run(snooze(now_ms() + DAY)).await, Reply::Ok, "a running thread snoozes");
    let snoozed_at = now_ms();
    session.emit(ProviderEvent::Request { id: "rpc-1".into(), item: None, body: approval("ls") });
    let t = w.wait_thread(&thread, |t| t.pending_requests == 1).await;
    assert!(t.inbox.snooze.is_none() && t.inbox.woke_at.is_some_and(|at| at >= snoozed_at), "{:?}", t.inbox);
    assert!(is_err(&w.run(snooze(now_ms() + DAY)).await, ErrorCode::Conflict));

    // So does a failure.
    answer(&w, &thread).await;
    assert_eq!(w.run(snooze(now_ms() + DAY)).await, Reply::Ok);
    session.emit(ProviderEvent::TurnEnded(TurnEnd::Failed("boom".into())));
    let t = w.wait_thread(&thread, |t| t.status == ThreadStatus::Error).await;
    assert!(t.inbox.snooze.is_none() && t.inbox.woke_at.is_some());

    // A message ends a snooze.
    assert_eq!(w.run(snooze(now_ms() + DAY)).await, Reply::Ok);
    w.send(&thread, "three", SendMode::Queue).await;
    assert!(w.wait_thread(&thread, |t| t.status == ThreadStatus::Running).await.inbox.snooze.is_none());
}

#[tokio::test]
async fn threads_settle_by_themselves_after_days_without_activity_or_when_their_change_ends() {
    let fake = FakeProvider::new("codex");
    let w = World::new(vec![fake]).await;
    async fn idle(w: &World, prompt: &str) -> ThreadId {
        let thread = w.thread("codex", Some(prompt), false).await;
        w.wait_thread(&thread, |t| t.status == ThreadStatus::Idle && t.run_count == 1).await;
        thread
    }
    let plain = idle(&w, "a").await;
    let kept = idle(&w, "b").await;
    let manual_ = idle(&w, "c").await;
    let archived = idle(&w, "d").await;
    let snoozed = idle(&w, "e").await;
    let last = w.state().await.runs(&plain)[0].completed_at.unwrap();
    assert_eq!(w.run(Command::ThreadUnsettle { thread: kept.clone() }).await, Reply::Ok);
    assert_eq!(w.run(Command::ThreadAutoSettle { thread: manual_.clone(), enabled: false }).await, Reply::Ok);
    assert!(w.state().await.thread(&manual_).unwrap().inbox.manual);
    assert_eq!(w.run(Command::ThreadArchive { thread: archived.clone() }).await, Reply::Ok);
    let wake = now_ms() + 5 * DAY;
    assert_eq!(w.run(Command::ThreadSnooze { thread: snoozed.clone(), until: wake }).await, Reply::Ok);

    let settle_of = |s: &Records, t: &ThreadId| s.thread(t).unwrap().inbox.settle;
    w.host.settle_round(last + 3 * DAY - 1).await;
    assert_eq!(settle_of(&w.state().await, &plain), None, "three days aren't up");
    w.host.settle_round(now_ms() + 3 * DAY).await;
    let s = w.state().await;
    let plain_settled = settle_of(&s, &plain);
    let Some(Settle::Settled { at, by: SettledBy::Inactivity }) = plain_settled else { panic!("{plain_settled:?}") };
    assert!(at >= last, "dated at its last activity");
    assert!(matches!(settle_of(&s, &kept), Some(Settle::Active { .. })));
    for t in [&manual_, &archived, &snoozed] {
        assert_eq!(settle_of(&s, t), None);
    }
    // A snooze that woke counts as activity.
    w.host.settle_round(wake).await;
    w.host.settle_round(wake + 3 * DAY - 1).await;
    assert_eq!(settle_of(&w.state().await, &snoozed), None);
    w.host.settle_round(wake + 3 * DAY).await;
    assert_eq!(settle_of(&w.state().await, &snoozed), Some(Settle::Settled { at: wake, by: SettledBy::Inactivity }));

    // The project's rules: a day, or never; landed changes settle unless that's off.
    let rules = |after_days, on_land| Command::ProjectUpdate {
        project: w.project.clone(),
        name: None,
        settings: Some(ProjectSettings { auto_settle: AutoSettle { after_days, on_land }, ..ProjectSettings::default() }),
    };
    assert!(is_err(&w.run(rules(Some(91), true)).await, ErrorCode::Invalid));
    assert!(is_err(&w.run(rules(Some(0), true)).await, ErrorCode::Invalid));
    let one = idle(&w, "f").await;
    let two = idle(&w, "g").await;
    assert_eq!(w.run(rules(None, false)).await, Reply::Ok);
    w.host.settle_round(now_ms() + 365 * DAY).await;
    assert_eq!(settle_of(&w.state().await, &one), None, "never");
    let ended = |thread: &ThreadId, landed| Command::ThreadChangeEnded { thread: thread.clone(), landed };
    assert_eq!(w.run(ended(&one, true)).await, Reply::Ok);
    assert_eq!(settle_of(&w.state().await, &one), None, "on_land is off");
    assert_eq!(w.run(ended(&one, false)).await, Reply::Ok);
    assert!(matches!(settle_of(&w.state().await, &one), Some(Settle::Settled { by: SettledBy::Closed, .. })));
    assert_eq!(w.run(rules(Some(1), true)).await, Reply::Ok);
    assert_eq!(w.run(ended(&two, true)).await, Reply::Ok);
    assert!(matches!(settle_of(&w.state().await, &two), Some(Settle::Settled { by: SettledBy::Landed, .. })));
    assert_eq!(w.run(ended(&kept, true)).await, Reply::Ok);
    assert!(matches!(settle_of(&w.state().await, &kept), Some(Settle::Active { .. })), "kept active by hand");
    let three = idle(&w, "h").await;
    w.host.settle_round(now_ms() + DAY + 60_000).await;
    assert!(matches!(settle_of(&w.state().await, &three), Some(Settle::Settled { by: SettledBy::Inactivity, .. })));
}

#[tokio::test]
async fn model_options_reach_the_session_and_follow_the_model() {
    // The fake's `fake-1` has reasoning (low, high) and a Fast Mode switch; `fake-2` has none.
    let fake = manual("codex", Capabilities { mode_switch: true, ..Capabilities::default() });
    let w = World::new(vec![fake.clone()]).await;
    let fast = |on: bool| ModelOptions::from([("fastMode".to_string(), OptionValue::Flag(on))]);
    let configure = |thread: &ThreadId, model: Option<&str>, options: Option<ModelOptions>| Command::ThreadConfigure {
        thread: thread.clone(),
        title: None,
        provider: None,
        model: model.map(String::from),
        effort: None,
        options,
        runtime_mode: None,
        interaction_mode: None,
    };
    let reply = w
        .run(Command::ThreadCreate {
            project: w.project.clone(),
            provider: ProviderId::codex(),
            model: Some("fake-1".into()),
            effort: Some("high".into()),
            options: fast(true),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            worktree: false,
            title: None,
            prompt: Some("go".into()),
            attachments: Vec::new(),
            orchestrator: false,
        })
        .await;
    let Reply::Created { id } = reply else { panic!("{reply:?}") };
    let thread = ThreadId(id);
    let first = fake.session(0).await;
    assert_eq!((first.config.effort.as_deref(), &first.config.options), (Some("high"), &fast(true)));
    turn(&w, &first, &thread, &[]).await;
    assert_eq!(w.wait_thread(&thread, |_| true).await.options, fast(true));

    // Options from a client are checked.
    let bad = ModelOptions::from([("effort".to_string(), OptionValue::Choice("high".into()))]);
    assert!(is_err(&w.run(configure(&thread, None, Some(bad))).await, ErrorCode::Invalid));

    // A model without Fast Mode drops it; picking it again with the option brings it back, and
    // the session that already runs it goes on.
    assert_eq!(w.run(configure(&thread, Some("fake-2"), None)).await, Reply::Ok);
    assert!(w.wait_thread(&thread, |t| t.model.as_deref() == Some("fake-2")).await.options.is_empty());
    assert_eq!(w.run(configure(&thread, Some("fake-1"), Some(fast(true)))).await, Reply::Ok);
    assert!(matches!(w.send(&thread, "again", SendMode::Queue).await, Reply::Created { .. }));
    turn(&w, &first, &thread, &[]).await;
    assert_eq!(sends(&first.calls()), ["go", "again"]);

    // Changing only an option starts the next turn in a session that has it.
    assert_eq!(w.run(configure(&thread, None, Some(fast(false)))).await, Reply::Ok);
    assert!(matches!(w.send(&thread, "slower", SendMode::Queue).await, Reply::Created { .. }));
    let second = fake.session(1).await;
    assert_eq!((second.config.model.as_deref(), &second.config.options), (Some("fake-1"), &fast(false)));
    assert!(first.calls().contains(&Call::Close));
    turn(&w, &second, &thread, &[]).await;
}
