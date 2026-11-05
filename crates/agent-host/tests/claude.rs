//! The Claude Code adapter against a fake `claude` that replays transcripts.
//!
//! This binary is its own fake: with `ROLL_FAKE_CLAUDE` set it acts as the CLI, writing the
//! recorded output and checking every line the adapter writes against the transcript (control
//! requests, prompts, permission answers); otherwise it runs the tests, pointing
//! `ProviderConfig.binary` at itself. A mismatch makes the fake exit with the reason on stderr,
//! which the adapter reports in `Exited`.
//!
//! Transcripts in `fixtures/claude/`:
//! - `t3/`: T3 Code's Claude replay fixtures (MIT,
//!   `apps/server/src/orchestration-v2/testkit/fixtures/*/claude_transcript.ndjson`), recorded
//!   at the Agent SDK's `query()` level: `emit_inbound` frames are the CLI's output lines,
//!   `permission.request`/`permission.response` become `can_use_tool` control messages,
//!   `query.open` checks the launch flags and `query.interrupt` an interrupt request. Each
//!   `runtime_exit` ends a launch, and the SDK's `session.fork` becomes the next launch's
//!   `--resume <source> --fork-session --resume-session-at` (Claude Code 2.1.310 takes these
//!   with `--session-id`, checked by hand against the real CLI).
//! - `recorded/`: the raw stdio of the real CLI (2.1.310), both directions
//!   (`{"dir": "in" | "out" | "stderr" | "exit", "line": …}`), account data and paths scrubbed.
//! - the rest: hand-written in the raw format, for what no recording covers.

use agent_host::provider::claude::Claude;
use agent_host::provider::{
    ForkAt, ForkFrom, McpServer, Provider, ProviderConfig, ProviderEvent, RollbackStyle, Session, SessionConfig, TurnEnd,
    TurnInput,
};
use agent_proto::{
    Attachment, AuthStatus, Decision, InteractionMode, NodeBody, NodeStatus, RequestBody, Response, RuntimeMode, UpdateRequired,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn main() {
    if let Ok(scripts) = std::env::var("ROLL_FAKE_CLAUDE") {
        fake::main(&scripts);
    }
    let tests: &[(&str, fn())] = &[
        ("replays_a_simple_answer", replays_a_simple_answer),
        ("replays_turns_in_one_session", replays_turns_in_one_session),
        ("steering_joins_the_running_turn", steering_joins_the_running_turn),
        ("interrupt_before_output_ends_the_turn", interrupt_before_output_ends_the_turn),
        ("interrupt_mid_tool_interrupts_the_tool", interrupt_mid_tool_interrupts_the_tool),
        ("declined_write_is_an_approval_with_a_diff", declined_write_is_an_approval_with_a_diff),
        ("approved_command_runs", approved_command_runs),
        ("read_tools_complete_with_output", read_tools_complete_with_output),
        ("subagents_hang_their_work_off_the_agent_call", subagents_hang_their_work_off_the_agent_call),
        ("error_results_fail_the_turn", error_results_fail_the_turn),
        ("bash_reports_output_and_exit", bash_reports_output_and_exit),
        ("command_output_keeps_escapes", command_output_keeps_escapes),
        ("mcp_tools_are_mcp_items", mcp_tools_are_mcp_items),
        ("real_cli_streams_text_and_reasoning", real_cli_streams_text_and_reasoning),
        ("real_cli_approval_interrupt_and_steer", real_cli_approval_interrupt_and_steer),
        ("real_cli_compacts_on_request", real_cli_compacts_on_request),
        ("compaction_failures_say_why", compaction_failures_say_why),
        ("questions_and_plan_approval", questions_and_plan_approval),
        ("todos_edits_and_notices", todos_edits_and_notices),
        ("crash_fails_the_turn_with_stderr", crash_fails_the_turn_with_stderr),
        ("rollback_resumes_at_the_kept_turn", rollback_resumes_at_the_kept_turn),
        ("forks_natively_at_a_turn", forks_natively_at_a_turn),
        ("forks_natively_at_a_prior_turn", forks_natively_at_a_prior_turn),
        ("rolls_back_inside_a_native_fork", rolls_back_inside_a_native_fork),
        ("launch_flags_follow_the_session_config", launch_flags_follow_the_session_config),
        ("auto_compact_window_and_cache_lifetime", auto_compact_window_and_cache_lifetime),
        ("info_reports_version_login_and_models", info_reports_version_login_and_models),
        ("info_without_the_cli", info_without_the_cli),
    ];
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        ran += 1;
        match std::panic::catch_unwind(test) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed.push(*name);
            }
        }
    }
    println!("\n{} passed; {} failed", ran - failed.len(), failed.len());
    if !failed.is_empty() {
        std::process::exit(1);
    }
}

fn block_on<F: std::future::Future<Output = ()>>(f: F) {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(f)
}

// ---------------------------------------------------------------------------------------------
// T3 Code's fixtures
// ---------------------------------------------------------------------------------------------

fn replays_a_simple_answer() {
    block_on(async {
        let mut r = Replay::t3("simple", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread 77171d01-fb4f-4dff-a961-d9dd334be93d",
            "model claude-sonnet-4-6",
            "completed msg_01CM449j71BXKjsRZZpW2z64:0: message fixture simple ok",
            "usage in=11796 out=6 cached=9455 cost=0.011703",
            "turn completed",
            "exited",
        ]);
    });
}

fn replays_turns_in_one_session() {
    block_on(async {
        let mut r = Replay::t3("multi_turn", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.send(1).await;
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread 01899f29-45f7-43c3-b963-0f9181ed013f",
            "model claude-sonnet-4-6",
            "completed msg_018QDgm81Jgo5XonK3Bm8dEc:0: message first fixture turn complete",
            "usage in=11795 out=7 cached=9455 cost=0.011714",
            "turn completed",
            "completed msg_01PifXzuLoccLsnMjRC9hXJ8:0: message second fixture turn complete",
            // The CLI reports the session's running cost; a turn's is the difference.
            "usage in=11814 out=7 cached=11792 cost=0.003723",
            "turn completed",
            "exited",
        ]);
    });
}

fn steering_joins_the_running_turn() {
    block_on(async {
        let mut r = Replay::t3("message_steering", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.steer(1).await;
        r.turn().await;
        r.close().await;
        // The aborted request's result doesn't end the turn; one turn carries both prompts.
        r.expect(&[
            "thread (new)",
            "thread 05f72f7d-81d5-4db9-a3cf-234191cf79f9",
            "model claude-sonnet-4-6",
            "completed msg_01Cjp5MnmN6F7wwrzNss9zVW:0: message steering fixture observed",
            "usage in=11805 out=6 cached=9455 cost=0.011737",
            "turn completed",
            "exited",
        ]);
    });
}

fn interrupt_before_output_ends_the_turn() {
    block_on(async {
        // The recorded CLI never wrote a result: its exit ends the interrupted turn.
        let mut r = Replay::start("t3/turn_interrupt", opts(RuntimeMode::FullAccess).model("claude-sonnet-4-6").exit_now()).await;
        r.send(0).await;
        r.session.interrupt().await.unwrap();
        r.until_exit().await;
        r.expect(&["thread (new)", "usage in=0 out=0 cached=0 cost=-", "turn interrupted", "exited"]);
    });
}

fn interrupt_mid_tool_interrupts_the_tool() {
    block_on(async {
        let mut r = Replay::start("t3/turn_interrupt_mid_tool", opts(RuntimeMode::FullAccess).model("claude-sonnet-4-6").exit_now()).await;
        r.send(0).await;
        r.until(|e| matches!(e, ProviderEvent::Item(i) if i.id.starts_with("toolu_"))).await;
        r.session.interrupt().await.unwrap();
        r.until_exit().await;
        r.expect(&[
            "thread (new)",
            "thread 7d29f78b-4ff3-4147-b35f-4d0f89c5d7f5",
            "model claude-sonnet-4-6",
            "completed msg_0134LoSqmeAtmiGfT8XBAnc4:0: reasoning The user wants me to run a specific comm…",
            r#"running toolu_01FAyzzWN34naNJKvPcD7B1D: tool Bash/command "node -e "console.log('interrupt fixture tool started'); setT…""#,
            r#"interrupted toolu_01FAyzzWN34naNJKvPcD7B1D: tool Bash/command "node -e "console.log('interrupt fixture tool started'); setT…""#,
            "usage in=0 out=0 cached=0 cost=-",
            "turn interrupted",
            "exited",
        ]);
    });
}

fn declined_write_is_an_approval_with_a_diff() {
    block_on(async {
        let mut r = Replay::t3("tool_call_denied_write", RuntimeMode::Ask).await;
        r.send(0).await;
        let request = r.request().await;
        let RequestBody::Approval(approval) = &request.1 else { panic!("{request:?}") };
        assert_eq!(approval.files.len(), 1);
        assert_eq!(approval.files[0].diff.as_deref(), Some("--- /dev/null\n+++ b/.claude-denied-write.txt\n@@ -0,0 +1,1 @@\n+approval fixture\n"));
        assert_eq!(approval.reason.as_deref(), Some(".claude-denied-write.txt"));
        r.respond(&request.0, Decision::Decline).await;
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread d745e7a5-b8b2-4bc8-aa7e-5072f50380d6",
            "model claude-sonnet-4-6",
            "running toolu_016StH6rgvnkDxU2MWt46gYE: tool Write/edit \".claude-denied-write.txt\" files=[.claude-denied-write.txt(add)]",
            "request perm_1 for toolu_016StH6rgvnkDxU2MWt46gYE: file_change \"Write .claude-denied-write.txt\" [accept, accept_for_session, decline, cancel]",
            "failed toolu_016StH6rgvnkDxU2MWt46gYE: tool Write/edit \".claude-denied-write.txt\" out=User declined tool execution. files=[.claude-denied-write.txt(add)]",
            "running msg_011Cf3qYNcr3jSaN9m685qT7:0: message ",
            "completed msg_011Cf3qYNcr3jSaN9m685qT7:0: message write permission denied",
            "usage in=43461 out=224 cached=35908 cost=0.060467",
            "turn completed",
            "exited",
        ]);
    });
}

fn approved_command_runs() {
    block_on(async {
        let mut r = Replay::t3("tool_call_read_only_on_request", RuntimeMode::Ask).await;
        r.send(0).await;
        let request = r.request().await;
        let RequestBody::Approval(approval) = &request.1 else { panic!("{request:?}") };
        assert_eq!(approval.command.as_deref(), Some("printf 'codex app-server approval fixture' > .codex-probe-write-action.txt"));
        assert_eq!(approval.cwd.as_deref(), Some("/tmp/claude-replay-tool_call_read_only_on_request"));
        r.respond(&request.0, Decision::Accept).await;
        r.turn().await;
        r.close().await;
        let tool = r.items("toolu_01CUcXE2dtk9uUvh3LpAWzhK");
        assert_eq!(tool.last().unwrap(), "completed toolu_01CUcXE2dtk9uUvh3LpAWzhK: tool Bash/command \"printf 'codex app-server approval fixture' > .codex-probe-wr…\" exit=0 out=(Bash completed with no output)");
        r.expect_tail(&["turn completed", "exited"]);
    });
}

fn read_tools_complete_with_output() {
    block_on(async {
        let mut r = Replay::t3("tool_call_read_only", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread 473fb58f-85a3-4f21-b9d3-b1fdf5ef41f9",
            "model claude-sonnet-4-6",
            "completed msg_01GA9LpBQUx46bYfJz1Dg3sx:0: reasoning Let me read both files simultaneously.",
            "completed msg_01GA9LpBQUx46bYfJz1Dg3sx:1: message I'll read both files simultaneously!",
            "running toolu_017Q89TYYCDJPT2uLttrCn3A: tool Read/read \"package.json\"",
            "running toolu_01CAafSU7YvuRYWdrFzpGCCW: tool Read/read \"tsconfig.json\"",
            r#"completed toolu_017Q89TYYCDJPT2uLttrCn3A: tool Read/read "package.json" out=1\t{\n2\t  "name": "claude-read-only-fixture",\n3\t  "privat…"#,
            r#"completed toolu_01CAafSU7YvuRYWdrFzpGCCW: tool Read/read "tsconfig.json" out=1\t{\n2\t  "compilerOptions": {\n3\t    "module": "ESNext",\…"#,
            "completed msg_017vbtgCJT7W7ayU6dpcREpU:0: message read only tool fixture complete",
            "usage in=6630 out=162 cached=6298 cost=0.005561",
            "turn completed",
            "exited",
        ]);
    });
}

fn subagents_hang_their_work_off_the_agent_call() {
    block_on(async {
        let mut r = Replay::t3("subagent", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        let a = "toolu_01FfHTrUeymLi4LfqJVmTZjz";
        let b = "toolu_0113VSWTCamvp1adXDZrK5NG";
        assert_eq!(r.items(a), [
            "running toolu_01FfHTrUeymLi4LfqJVmTZjz: subagent general-purpose prompt=Read the file `package.json` in the curr…",
            // `task_progress` says what it's doing while it runs (T3's subagent progress)…
            "running toolu_01FfHTrUeymLi4LfqJVmTZjz: subagent general-purpose prompt=Read the file `package.json` in the curr… progress=Reading package.json",
            "running toolu_01FfHTrUeymLi4LfqJVmTZjz: subagent general-purpose prompt=Read the file `package.json` in the curr… progress=Summarizing package.json",
            // …and its answer replaces that when it's done.
            "completed toolu_01FfHTrUeymLi4LfqJVmTZjz: subagent general-purpose prompt=Read the file `package.json` in the curr… result=Here are the full contents of `package.j…",
        ]);
        assert_eq!(r.items(b).len(), 3);
        // The subagents' own tool calls hang off their Agent call.
        assert_eq!(r.items("toolu_01EV8j88CPEmhoWF3iuvaFSC").last().unwrap(), &format!("completed toolu_01EV8j88CPEmhoWF3iuvaFSC ^{a}: tool Read/read \"/private/var/folders/b3/b51_pdxj7dl0t981zpqkxxhr0000gn/T/t3-…\" out=1\\t{{\"name\":\"claude-read-only-fixture\",\"private\":true,\"script…"));
        assert_eq!(r.items("toolu_01P6KQcUPy8t2bCbFjv8K4Jz").last().unwrap().split(':').next().unwrap(), format!("completed toolu_01P6KQcUPy8t2bCbFjv8K4Jz ^{b}"));
        // The agentId trailer isn't part of the answer.
        let result = r.events.iter().rev().find_map(|e| match e {
            ProviderEvent::Item(i) if i.id == a => match &i.body {
                NodeBody::Subagent { result, .. } => result.clone(),
                _ => None,
            },
            _ => None,
        });
        assert!(result.unwrap().ends_with("runs the TypeScript compiler in no-emit mode."));
        r.expect_tail(&["turn completed", "exited"]);
    });
}

fn error_results_fail_the_turn() {
    block_on(async {
        let mut r = Replay::t3("claude_result_is_error", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.send(1).await;
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread aa401aa4-e597-4e40-8e40-1e401e401e40",
            "model claude-sonnet-4-6",
            "completed 7f28a402-6d4c-491e-abbb-2b3c231a06f0:0: message Failed to authenticate. API Error: 401 Invalid authenticatio…",
            "usage in=0 out=0 cached=0 cost=0.000000",
            "turn failed: Failed to authenticate. API Error: 401 Invalid authentication credentials",
            "completed msg_01iserrorfixture1:0: message claude result is_error fixture recovered",
            "usage in=2 out=2 cached=0 cost=0.001000",
            "turn completed",
            "exited",
        ]);
    });
}

fn bash_reports_output_and_exit() {
    block_on(async {
        let mut r = Replay::t3("claude_local_bash_task", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        assert_eq!(r.items("toolu_01QHaWEM6vnYqxYPvpqv6Xq4"), [
            "running toolu_01QHaWEM6vnYqxYPvpqv6Xq4: tool Bash/command \"vp run --filter @t3tools/web typecheck 2>&1 | tail -30\"",
            "completed toolu_01QHaWEM6vnYqxYPvpqv6Xq4: tool Bash/command \"vp run --filter @t3tools/web typecheck 2>&1 | tail -30\" exit=0 out=~/apps/web$ tsgo --noEmit cache disabled",
        ]);
        r.expect_tail(&["completed msg_01localbashfixture2:0: message claude local bash task fixture complete", "usage in=2 out=2 cached=0 cost=0.001000", "turn completed", "exited"]);
    });
}

fn command_output_keeps_escapes() {
    block_on(async {
        let mut r = Replay::t3("command_output_streaming", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        let output = r.events.iter().rev().find_map(|e| match e {
            ProviderEvent::Item(i) if i.id == "toolu_01CJnQSvsvh3iZtmvSWQVVvg" => match &i.body {
                NodeBody::Tool(t) => t.output.clone(),
                _ => None,
            },
            _ => None,
        });
        assert!(output.unwrap().starts_with("\u{1b}[32mstream line 1\u{1b}[0m\n"));
        r.expect_tail(&["turn completed", "exited"]);
    });
}

fn mcp_tools_are_mcp_items() {
    block_on(async {
        let mut r = Replay::t3("claude_mcp_tool_presentation", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        let first = r.items("toolu_01CqVNFCESeShHVUiLMLPsCg");
        assert!(first[0].starts_with("running toolu_01CqVNFCESeShHVUiLMLPsCg: tool mcp__claude_ai_Firecrawl__firecrawl_scrape/mcp \"claude_ai_Firecrawl: firecrawl_scrape\""), "{first:?}");
        assert!(first[1].starts_with("completed "), "{first:?}");
        r.expect_tail(&["completed msg_011CfXyN16JbrX8sT6DrKgDY:0: message mcp presentation fixture complete", "usage in=405406 out=289 cached=390553 cost=0.211105", "turn completed", "exited"]);
    });
}

// ---------------------------------------------------------------------------------------------
// The real CLI's own stdio
// ---------------------------------------------------------------------------------------------

fn real_cli_streams_text_and_reasoning() {
    block_on(async {
        let mut r = Replay::start("recorded/basic", opts(RuntimeMode::Ask).model("claude-haiku-5-5").effort("low")).await;
        r.send_text("Reply with exactly: ok").await;
        r.turn().await;
        r.send_text("Use the Bash tool to run exactly `echo roll-fixture` and then reply with exactly: done").await;
        r.turn().await;
        r.close().await;
        r.with_deltas().expect(&[
            "thread (new)",
            "thread 46fb3af2-e190-4038-ac02-3942197a34c9",
            "model claude-haiku-5-5",
            "running msg_011CggSsPMi1uT8KzLBJ8Scd:0: message ",
            "delta msg_011CggSsPMi1uT8KzLBJ8Scd:0 ok",
            "completed msg_011CggSsPMi1uT8KzLBJ8Scd:0: message ok",
            "usage in=25625 out=4 cached=0 cost=0.005295",
            "turn completed",
            "running msg_011CggSsaZPuCH4t5L5PR3bD:0: reasoning ",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0 I need",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0  to call the",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0  Bash tool here",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0 , which is the m",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0 cp__dish_young__",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0 dilemma_Bash",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0  tool,",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0  so I'll use it.",
            "delta msg_011CggSsaZPuCH4t5L5PR3bD:0 \\n\\n",
            "completed msg_011CggSsaZPuCH4t5L5PR3bD:0: reasoning I need to call the Bash tool here, which…",
            "running toolu_01Q1hBcvzsbU1PFtVsce5NLn: tool Bash/command \"echo roll-fixture\"",
            "completed toolu_01Q1hBcvzsbU1PFtVsce5NLn: tool Bash/command \"echo roll-fixture\" exit=0 out=roll-fixture",
            "running msg_011CggSsj8baTw5MrTEACBdW:0: message ",
            "delta msg_011CggSsj8baTw5MrTEACBdW:0 done",
            "completed msg_011CggSsj8baTw5MrTEACBdW:0: message done",
            "usage in=51563 out=140 cached=38138 cost=0.003136",
            "turn completed",
            "exited",
        ]);
        // One report per main-agent message as it streams in, none repeated.
        assert_eq!(
            r.with_context().rendered().into_iter().filter(|l| l.starts_with("context")).collect::<Vec<_>>(),
            ["context 25629/1000000", "context 25696/1000000", "context 25830/1000000", "context 25873/1000000"]
        );
        let args = r.launches().remove(0);
        for flags in [
            &["--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--include-partial-messages"][..],
            &["--permission-prompt-tool", "stdio", "--permission-mode", "default"],
            &["--model", "claude-haiku-5-5"],
            &["--effort", "low"],
            &["--thinking", "adaptive", "--thinking-display", "summarized"],
            &["--settings", r#"{"showThinkingSummaries":true}"#],
        ] {
            assert!(has_flags(&args, flags), "{flags:?} not in {args:?}");
        }
        assert!(args.last().unwrap().starts_with("--session-id="));
    });
}

/// T3's `/compact` on the real CLI: the compaction is a turn of its own; its row carries the
/// context before and after, the context drops, and the result's text is no reply.
fn real_cli_compacts_on_request() {
    block_on(async {
        let mut r = Replay::start("recorded/compact", opts(RuntimeMode::Ask).model("claude-haiku-5-5").effort("low")).await;
        r.send_text("Reply with exactly: ok").await;
        r.turn().await;
        r.session.compact().await.unwrap();
        assert_eq!(r.turn().await, TurnEnd::Completed);
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread 66f90b73-1efb-45ef-a55d-762c78b8fe11",
            "model claude-haiku-5-5",
            "running msg_011CgjM2RuKZRYaZRJmjuoBE:0: message ",
            "completed msg_011CgjM2RuKZRYaZRJmjuoBE:0: message ok",
            "usage in=25603 out=4 cached=0 cost=0.005291",
            "turn completed",
            "running compaction:1: compaction ?→?",
            "completed compaction:1: compaction 25607→7378",
            "usage in=0 out=0 cached=0 cost=0.003752",
            "turn completed",
            "exited",
        ]);
        let context: Vec<String> = r.with_context().rendered().into_iter().filter(|l| l.starts_with("context")).collect();
        assert_eq!(context.last().map(String::as_str), Some("context 7378/1000000"));
    });
}

/// A compaction that fails says why on its row; a `/compact` with nothing to compact has no
/// compaction frames at all, only its result's text, which becomes the row's reason.
fn compaction_failures_say_why() {
    block_on(async {
        let mut r = Replay::start("compact_failed", opts(RuntimeMode::FullAccess)).await;
        r.session.compact().await.unwrap();
        r.turn().await;
        r.session.compact().await.unwrap();
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread 11111111-2222-4333-8444-555555555555",
            "model claude-opus-5-5",
            "running compaction:1: compaction ?→?",
            "failed compaction:1: compaction ?→? error=API Error: 529 overloaded_error",
            "usage in=0 out=0 cached=0 cost=0.000000",
            "turn completed",
            "failed compaction:2: compaction ?→? error=Not enough messages to compact.",
            "usage in=0 out=0 cached=0 cost=0.000000",
            "turn completed",
            "exited",
        ]);
    });
}

fn real_cli_approval_interrupt_and_steer() {
    block_on(async {
        let resume = "46fb3af2-e190-4038-ac02-3942197a34c9";
        let mut r = Replay::start("recorded/interrupt_steer", opts(RuntimeMode::Ask).model("claude-haiku-5-5").effort("low").resume(resume)).await;
        r.send_text("Use the Bash tool to run exactly `sleep 20 && touch roll-fixture.txt` and then reply with exactly: done").await;
        let (id, body) = r.request().await;
        let RequestBody::Approval(approval) = &body else { panic!("{body:?}") };
        assert_eq!(approval.title, "Run `sleep 20 && touch roll-fixture.txt`");
        assert_eq!(approval.reason.as_deref(), Some("Wait 20 seconds then create roll-fixture.txt"));
        r.respond(&id, Decision::Accept).await;
        r.session.interrupt().await.unwrap();
        r.turn().await;
        r.send_text("Reply with exactly: one, then count slowly from 1 to 30 in words").await;
        r.until(|e| matches!(e, ProviderEvent::Item(i) if matches!(i.body, NodeBody::Reasoning { .. }))).await;
        r.session.steer(TurnInput { text: "Stop. Instead reply with exactly: two".into(), attachments: vec![] }).await.unwrap();
        r.turn().await;
        r.close().await;
        r.expect(&[
            &format!("thread {resume}"),
            "model claude-haiku-5-5",
            "running toolu_01GVtzF2BqqoGQxXtV4mPbTz: tool Bash/command \"sleep 20 && touch roll-fixture.txt\"",
            &format!("request {id} for toolu_01GVtzF2BqqoGQxXtV4mPbTz: command \"Run `sleep 20 && touch roll-fixture.txt`\" [accept, accept_for_session, decline, cancel]"),
            "interrupted toolu_01GVtzF2BqqoGQxXtV4mPbTz: tool Bash/command \"sleep 20 && touch roll-fixture.txt\" out=The user doesn't want to proceed with this tool use. The too…",
            "usage in=25942 out=118 cached=25868 cost=0.000332",
            "turn interrupted",
            // The steer aborts the request that was thinking; the CLI starts over with both inputs.
            "running msg_011CggSx8wAu446zCgABjacc:0: reasoning ",
            "interrupted msg_011CggSx8wAu446zCgABjacc:0: reasoning I'm going to read the request as wanting…",
            "running msg_011CggSxLKHTGDRdgnZ2QL5D:0: reasoning ",
            "completed msg_011CggSxLKHTGDRdgnZ2QL5D:0: reasoning I'm checking the latest instruction, whi…",
            "running msg_011CggSxLKHTGDRdgnZ2QL5D:1: message ",
            "completed msg_011CggSxLKHTGDRdgnZ2QL5D:1: message two",
            "usage in=26269 out=20 cached=26060 cost=0.000312",
            "turn completed",
            "exited",
        ]);
        let args = r.launches().remove(0);
        assert_eq!(args.last().map(String::as_str), Some(&*format!("--resume={resume}")));
    });
}

// ---------------------------------------------------------------------------------------------
// Hand-written transcripts
// ---------------------------------------------------------------------------------------------

fn questions_and_plan_approval() {
    block_on(async {
        let mut r = Replay::start("plan_mode", opts(RuntimeMode::AutoEdit).plan()).await;
        r.send_text("Plan a cache layer").await;
        let (id, body) = r.request().await;
        let RequestBody::Questions { questions } = &body else { panic!("{body:?}") };
        assert_eq!(questions[1].header, "Layers");
        assert!(questions[1].multi);
        let answers = BTreeMap::from([
            ("Which store should back the cache?".to_string(), "Redis".to_string()),
            ("Which layers?".to_string(), "API, DB".to_string()),
        ]);
        r.session.respond(&id, &Response::Answers { answers }).await.unwrap();
        let (id, body) = r.request().await;
        let RequestBody::Approval(approval) = &body else { panic!("{body:?}") };
        assert_eq!(approval.decisions, [Decision::Accept, Decision::Decline, Decision::Cancel]);
        // Approving the plan leaves plan mode for the thread's runtime mode (auto_edit).
        r.respond(&id, Decision::Accept).await;
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread 11111111-2222-4333-8444-555555555555",
            "model claude-opus-5-5",
            "running toolu_q: tool AskUserQuestion/other \"Which store should back the cache?\"",
            "request q1 for toolu_q: questions [Store: Which store should back the cache? (Redis | In memory)] [Layers: Which layers? (API | DB) multi]",
            "completed toolu_q: tool AskUserQuestion/other \"Which store should back the cache?\" out=User has answered your questions.",
            "running toolu_p: plan # Cache\\n1. Add Redis\\n2. Cache API and DB reads",
            "request p1 for toolu_p: plan \"Carry out the plan\" [accept, decline, cancel]",
            "completed toolu_p: plan # Cache\\n1. Add Redis\\n2. Cache API and DB reads",
            "completed msg_d:0: message Starting on the cache.",
            "usage in=110 out=20 cached=100 cost=0.010000",
            "turn completed",
            "exited",
        ]);
        assert!(has_flags(&r.launches()[0], &["--permission-mode", "plan"]));
    });
}

fn todos_edits_and_notices() {
    block_on(async {
        let mut r = Replay::start("todos_edits", opts(RuntimeMode::Ask)).await;
        r.send_text("Refactor lib.rs").await;
        let (id, body) = r.request().await;
        let RequestBody::Approval(approval) = &body else { panic!("{body:?}") };
        assert_eq!(approval.files[0].diff.as_deref(), Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n fn a() {}\n+fn b() {}\n"));
        // Accept for the session: Claude's own suggestions, kept for this session only.
        r.respond(&id, Decision::AcceptForSession).await;
        r.until(|e| matches!(e, ProviderEvent::RequestCancelled { .. })).await;
        r.turn().await;
        r.close().await;
        r.expect(&[
            "thread (new)",
            "thread 11111111-2222-4333-8444-555555555555",
            "model claude-opus-5-5",
            "running toolu_t: todos [x] Read the code; [~] Edit lib.rs; [ ] Run the tests",
            "completed toolu_t: todos [x] Read the code; [~] Edit lib.rs; [ ] Run the tests",
            "running toolu_e: tool Edit/edit \"src/lib.rs\" files=[src/lib.rs(modify)]",
            "request e1 for toolu_e: file_change \"Edit src/lib.rs\" [accept, accept_for_session, decline, cancel]",
            "completed toolu_e: tool Edit/edit \"src/lib.rs\" out=The file src/lib.rs has been updated. files=[src/lib.rs(modify)]",
            "running compaction:1: compaction ?→?",
            "completed compaction:1: compaction 150000→38000",
            "completed retry:retry1: system Retrying after an API error (529 overloaded_error), attempt 1 of 10",
            "failed hook:h1: system Hook PostToolUse:Edit failed: lint failed",
            "running toolu_w: tool WebFetch/web \"https://example.com\"",
            "request w1 for toolu_w: tool \"Use Fetch: https://example.com\" [accept, accept_for_session, decline, cancel]",
            "withdrawn w1",
            "failed toolu_w: tool WebFetch/web \"https://example.com\" out=Permission request was cancelled",
            "completed msg_f:0: message Done.",
            "usage in=30 out=5 cached=0 cost=0.020000",
            "turn completed",
            "exited",
        ]);
        // The context is the main agent's last message (input, cache writes and reads, output)
        // against Opus 5.5's 1M window; a compaction drops it to what's left.
        assert_eq!(
            r.with_context().rendered().into_iter().filter(|l| l.starts_with("context")).collect::<Vec<_>>(),
            ["context 142123/1000000", "context 149805/1000000", "context 38000/1000000", "context 38209/1000000"]
        );
        // The CLI's own patch replaces the input's: real line numbers.
        let diff = r.events.iter().rev().find_map(|e| match e {
            ProviderEvent::Item(i) if i.id == "toolu_e" => match &i.body {
                NodeBody::Tool(t) => t.files[0].diff.clone(),
                _ => None,
            },
            _ => None,
        });
        assert_eq!(diff.as_deref(), Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -3,1 +3,2 @@\n fn a() {}\n+fn b() {}\n"));
    });
}

fn crash_fails_the_turn_with_stderr() {
    block_on(async {
        let mut r = Replay::start("crash", opts(RuntimeMode::FullAccess).exit_now()).await;
        r.send_text("Run the tests").await;
        r.until_exit().await;
        r.expect(&[
            "thread (new)",
            "thread 11111111-2222-4333-8444-555555555555",
            "model claude-opus-5-5",
            "running toolu_b: tool Bash/command \"cargo test\"",
            "failed toolu_b: tool Bash/command \"cargo test\"",
            "usage in=0 out=0 cached=0 cost=-",
            "turn failed: Claude Code exited (exit status: 2): FATAL ERROR: out of memory",
            "exited: Claude Code exited (exit status: 2): FATAL ERROR: out of memory",
        ]);
    });
}

fn rollback_resumes_at_the_kept_turn() {
    block_on(async {
        let mut r = Replay::start("rollback_1.ndjson,rollback_2", opts(RuntimeMode::FullAccess)).await;
        assert_eq!(r.claude.rollback_style(), RollbackStyle::ResumeAt);
        r.send_text("one").await;
        r.turn().await;
        r.send_text("two").await;
        r.turn().await;
        assert!(r.session.rollback(1).await.is_err(), "Claude rolls back by resuming, not in a session");
        r.close().await;
        assert_eq!(r.turn_refs(), ["a-1", "a-2"]);

        // Another session (another process, after a restart too) resumes at the kept turn's ref.
        r.resume_at("11111111-2222-4333-8444-555555555555", "a-1").await;
        r.session.set_modes(RuntimeMode::Ask, InteractionMode::Default).await.unwrap();
        r.send_text("three").await;
        r.turn().await;
        r.close().await;
        let launches = r.launches();
        assert_eq!(launches.len(), 2);
        assert!(has_flags(&launches[1], &["--permission-mode", "bypassPermissions"]));
        assert!(has_flags(&launches[1], &["--resume=11111111-2222-4333-8444-555555555555", "--resume-session-at=a-1"]), "{:?}", launches[1]);
        assert!(!launches[1].iter().any(|a| a.starts_with("--session-id") || a == "--fork-session"), "{:?}", launches[1]);
        r.expect_tail(&["completed msg_3:0: message three", "usage in=1 out=1 cached=0 cost=-", "turn completed", "exited"]);
        assert_eq!(r.turn_refs(), ["a-1", "a-2", "a-3"], "the new turn goes past the kept one");
    });
}

// ---------------------------------------------------------------------------------------------
// Native forks (T3's `thread_fork_native*` fixtures)
// ---------------------------------------------------------------------------------------------

const FORK_SOURCE: &str = "8d0dafab-45d3-45eb-abef-41b9e55d5ff9";

fn forks_natively_at_a_turn() {
    block_on(async {
        let mut r = Replay::t3("thread_fork_native", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        let refs = r.turn_refs();
        assert_eq!(refs, ["68baa075-b7d4-4689-866b-96bd51e570c3"]);

        // Claude names fork points by message: it can't drop turns it has no message for.
        let drop = SessionConfig { fork: Some(ForkFrom { thread: FORK_SOURCE.into(), at: ForkAt::DropLast(1) }), ..r.cfg.clone() };
        assert!(r.claude.start(drop).await.is_err());
        assert_eq!(r.launches().len(), 1, "refused before launching");

        r.fork(ForkFrom { thread: FORK_SOURCE.into(), at: ForkAt::Turn(refs[0].clone()) }).await;
        r.send(1).await;
        r.turn().await;
        r.close().await;
        let launches = r.launches();
        assert_eq!(launches.len(), 2);
        assert!(has_flags(&launches[1], &[&format!("--resume={FORK_SOURCE}"), "--fork-session"]), "{:?}", launches[1]);
        assert!(has_flags(&launches[1], &["--resume-session-at=68baa075-b7d4-4689-866b-96bd51e570c3"]));
        // The copy runs under an id of ours, known before its first turn.
        assert!(launches[1].iter().any(|a| a.starts_with("--session-id=")) && !launches[1].contains(&format!("--session-id={FORK_SOURCE}")));
        r.with_refs().expect(&[
            "thread (new)",
            &format!("thread {FORK_SOURCE}"),
            "model claude-sonnet-4-6",
            "completed msg_01RzC6FfR9RGxxVcrpBK6duZ:0: message source fork seed ok",
            "usage in=11797 out=7 cached=9455 cost=0.011722",
            "ref 68baa075-b7d4-4689-866b-96bd51e570c3",
            "turn completed",
            "exited",
            "thread (new)",
            "model claude-sonnet-4-6",
            "completed msg_01MvGdK8D9o71fU1raKAbM69:0: message fork native ok",
            "usage in=11817 out=6 cached=11794 cost=0.003712",
            "ref b800a4dd-77a5-4e68-acac-1c651a50a02e",
            "turn completed",
            "exited",
        ]);
    });
}

fn forks_natively_at_a_prior_turn() {
    block_on(async {
        let mut r = Replay::t3("thread_fork_native_prior_turn", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.send(1).await;
        r.turn().await;
        r.close().await;
        let refs = r.turn_refs();
        assert_eq!(refs, ["be0e3276-8c27-44bf-8ed0-0b4424e9f902", "c517b74b-9537-4260-afcb-186c4056c0a4"]);
        r.fork(ForkFrom { thread: "a91391e1-0de3-4107-a7c9-db7b1bbd8b99".into(), at: ForkAt::Turn(refs[0].clone()) }).await;
        r.send(2).await;
        r.turn().await;
        r.close().await;
        assert!(has_flags(&r.launches()[1], &["--resume-session-at=be0e3276-8c27-44bf-8ed0-0b4424e9f902"]));
        // The copy only holds the first turn.
        let last = r.last_message();
        assert!(last.contains("fork boundary alpha") && !last.contains("beta"), "{last}");
        r.expect_tail(&["turn completed", "exited"]);
    });
}

fn rolls_back_inside_a_native_fork() {
    block_on(async {
        let mut r = Replay::t3("thread_fork_native_fork_local_rollback", RuntimeMode::FullAccess).await;
        r.send(0).await;
        r.turn().await;
        r.close().await;
        let at = r.turn_refs().remove(0);
        r.fork(ForkFrom { thread: "273fe17b-65fb-451f-ad99-ff19d323852d".into(), at: ForkAt::Turn(at) }).await;
        r.send(1).await;
        r.turn().await;
        r.send(2).await;
        r.turn().await;
        r.close().await;
        // Back to the fork's first turn, in the copy (its id is ours), not the source.
        let ours = r.launches()[1].iter().find_map(|a| a.strip_prefix("--session-id=")).unwrap().to_string();
        let first = r.turn_refs()[1].clone();
        assert_eq!(first, "14310fd5-a1aa-448b-8880-de52f86541b1");
        r.resume_at(&ours, &first).await;
        r.send(3).await;
        r.turn().await;
        r.close().await;
        let launches = r.launches();
        assert_eq!(launches.len(), 3);
        assert!(has_flags(&launches[2], &[&format!("--resume={ours}"), "--resume-session-at=14310fd5-a1aa-448b-8880-de52f86541b1"]), "{:?}", launches[2]);
        let last = r.last_message();
        assert!(last.contains("fork local first") && !last.contains("fork local second"), "{last}");
        r.expect_tail(&["turn completed", "exited"]);
    });
}

/// T3's "Auto-compact after": `autoCompactWindow` in the CLI's settings, and the meter's threshold
/// 33,000 below it (only under the model's window). The real CLI's usage says which prompt cache
/// the requests wrote (1-hour entries here, a subscription): the lifetime idle compaction keys off.
fn auto_compact_window_and_cache_lifetime() {
    block_on(async {
        let mut o = opts(RuntimeMode::Ask).model("claude-haiku-5-5").effort("low");
        o.auto_compact_window = Some(150_000);
        let mut r = Replay::start("recorded/basic", o).await;
        r.send_text("Reply with exactly: ok").await;
        r.turn().await;
        r.close().await;
        let args = r.launches().remove(0);
        assert!(has_flags(&args, &["--settings", r#"{"autoCompactWindow":150000,"showThinkingSummaries":true}"#]), "{args:?}");
        let contexts: Vec<agent_proto::ContextUsage> = r.events.iter().filter_map(|e| if let ProviderEvent::Context(c) = e { Some(*c) } else { None }).collect();
        assert!(!contexts.is_empty());
        for c in contexts {
            assert_eq!((c.cache_ttl_ms, c.auto_compact_at, c.max_tokens), (Some(3_600_000), Some(117_000), Some(1_000_000)), "{c:?}");
        }

        // A window at or over the model's never applies, so the meter says nothing about it.
        let mut o = opts(RuntimeMode::Ask).model("claude-haiku-5-5").effort("low");
        o.auto_compact_window = Some(1_000_000);
        let mut r = Replay::start("recorded/basic", o).await;
        r.send_text("Reply with exactly: ok").await;
        r.turn().await;
        r.close().await;
        assert!(r.events.iter().any(|e| matches!(e, ProviderEvent::Context(c) if c.auto_compact_at.is_none())));
    });
}

fn launch_flags_follow_the_session_config() {
    block_on(async {
        let mut o = opts(RuntimeMode::AutoEdit).model("claude-opus-5-5").effort("ultracode");
        o.options = agent_proto::ModelOptions::from([("fastMode".into(), agent_proto::OptionValue::Flag(true))]);
        o.instructions = Some("Keep the code rolling.".into());
        o.mcp = vec![McpServer { name: "roll".into(), url: "http://localhost:9/mcp".into(), headers: vec![("Authorization".into(), "Bearer rlx_thread_secret".into())] }];
        o.env = vec![("ROLL_FAKE_SESSION_ENV".into(), "1".into())];
        let mut r = Replay::start("attachments", o).await;
        let image = Attachment { name: "a.png".into(), mime: "image/png".into(), data: "aGk=".into() };
        r.session.send(TurnInput { text: "Look".into(), attachments: vec![image] }).await.unwrap();
        r.turn().await;
        let bad = Attachment { name: "a.zip".into(), mime: "application/zip".into(), data: "aGk=".into() };
        assert!(r.session.send(TurnInput { text: "zip".into(), attachments: vec![bad] }).await.is_err());
        // roll's MCP server and its bearer token are in a private file, never on the command
        // line (`ps` shows every user's argv).
        let args = r.launches().remove(0);
        assert!(args.iter().all(|a| !a.contains("rlx_thread_secret")), "{args:?}");
        let at = args.iter().position(|a| a == "--mcp-config").expect("--mcp-config");
        let file = std::path::PathBuf::from(&args[at + 1]);
        let mode = |p: &std::path::Path| std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions()) & 0o777;
        assert_eq!((mode(&file), mode(file.parent().unwrap())), (0o600, 0o700), "{}", file.display());
        let config: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let want = json!({"mcpServers": {"roll": {"headers": {"Authorization": "Bearer rlx_thread_secret"}, "type": "http", "url": "http://localhost:9/mcp"}}});
        assert_eq!(config, want);
        r.close().await;
        assert!(!file.parent().unwrap().exists(), "the file goes with the session");
        let args = &r.launches()[0];
        assert!(has_flags(args, &["--permission-mode", "acceptEdits"]));
        assert!(has_flags(args, &["--effort", "xhigh"]));
        // Ultracode and Fast Mode go in the settings (T3's `compileClaudeModelSelection`).
        assert!(has_flags(args, &["--settings", r#"{"fastMode":true,"showThinkingSummaries":true,"ultracode":true}"#]), "{args:?}");
        assert!(has_flags(args, &["--append-system-prompt", "Keep the code rolling."]));
        assert!(has_flags(args, &["--allowedTools", "mcp__roll__*"]), "{args:?}");
        r.expect_tail(&["turn completed", "exited"]);
    });
}

fn info_reports_version_login_and_models() {
    block_on(async {
        let claude = Claude::new(ProviderConfig {
            binary: Some(std::env::current_exe().unwrap()),
            args: vec![],
            env: vec![
                ("ROLL_FAKE_CLAUDE".into(), "probe".into()),
                ("ROLL_FAKE_ACCOUNT".into(), json!({"tokenSource": "claude.ai", "email": "dev@example.com", "subscriptionType": "max"}).to_string()),
                ("ROLL_FAKE_VERSION".into(), "2.1.290 (Claude Code)".into()),
                ("ANTHROPIC_BASE_URL".into(), "https://api.anthropic.com".into()),
            ],
            ..Default::default()
        });
        let info = claude.info().await;
        assert!(info.installed);
        assert_eq!(info.version.as_deref(), Some("2.1.290"));
        assert_eq!(info.auth, AuthStatus::Ready);
        assert_eq!(info.detail.as_deref(), Some("Logged in as dev@example.com (max)"));
        // Haiku 5.5 needs Claude Code 2.1.293: offered after an update (T3's update-required
        // models). Legacy models follow the current ones.
        let current: Vec<&str> = info.models.iter().filter(|m| !m.legacy).map(|m| m.id.as_str()).collect();
        assert_eq!(current, ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1"]);
        assert!(info.models.iter().skip(current.len()).all(|m| m.legacy) && info.models.iter().any(|m| m.id == "claude-haiku-4-5"));
        assert_eq!(info.update_required, [UpdateRequired { id: "claude-haiku-5-5".into(), name: "Claude Haiku 5.5".into(), min_version: "2.1.293".into() }]);
        assert!(info.capabilities.steer && info.capabilities.mode_switch && info.capabilities.rollback && info.capabilities.fork);

        let signed_out = Claude::new(ProviderConfig {
            binary: Some(std::env::current_exe().unwrap()),
            args: vec![],
            env: vec![
                ("ROLL_FAKE_CLAUDE".into(), "probe".into()),
                ("ROLL_FAKE_ACCOUNT".into(), json!({"tokenSource": "none", "apiKeySource": "none"}).to_string()),
                ("ANTHROPIC_BASE_URL".into(), "https://api.anthropic.com".into()),
            ],
            ..Default::default()
        });
        let info = signed_out.info().await;
        assert_eq!(info.auth, AuthStatus::Missing);
    });
}

fn info_without_the_cli() {
    block_on(async {
        let claude = Claude::new(ProviderConfig { binary: Some("/nonexistent/claude".into()), args: vec![], env: vec![], ..Default::default() });
        let info = claude.info().await;
        assert!(!info.installed);
        assert_eq!(info.auth, AuthStatus::Unknown);
        assert!(!info.models.is_empty());
    });
}

// ---------------------------------------------------------------------------------------------
// Driving a session
// ---------------------------------------------------------------------------------------------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude")
}

fn opts(runtime: RuntimeMode) -> Opts {
    Opts { runtime, ..Opts::default() }
}

#[derive(Default)]
struct Opts {
    runtime: RuntimeMode,
    interaction: InteractionMode,
    model: Option<String>,
    effort: Option<String>,
    options: agent_proto::ModelOptions,
    resume: Option<String>,
    fork: Option<ForkFrom>,
    instructions: Option<String>,
    mcp: Vec<McpServer>,
    env: Vec<(String, String)>,
    exit_now: bool,
    /// The machine's "Auto-compact after" (`ProviderConfig::auto_compact_window`).
    auto_compact_window: Option<u64>,
}

impl Opts {
    fn model(mut self, m: &str) -> Self {
        self.model = Some(m.into());
        self
    }
    fn effort(mut self, e: &str) -> Self {
        self.effort = Some(e.into());
        self
    }
    fn resume(mut self, id: &str) -> Self {
        self.resume = Some(id.into());
        self
    }
    fn plan(mut self) -> Self {
        self.interaction = InteractionMode::Plan;
        self
    }
    /// The fake exits as soon as its script ends, instead of when the session closes.
    fn exit_now(mut self) -> Self {
        self.exit_now = true;
        self
    }
}

struct Replay {
    claude: Claude,
    cfg: SessionConfig,
    session: Box<dyn Session>,
    rx: tokio::sync::mpsc::Receiver<ProviderEvent>,
    events: Vec<ProviderEvent>,
    prompts: Vec<String>,
    argv: PathBuf,
    deltas: bool,
    context: bool,
    refs: bool,
    _dir: tempfile::TempDir,
    cwd: PathBuf,
}

impl Replay {
    /// A T3 transcript, with the model and cwd its recording used.
    async fn t3(name: &str, runtime: RuntimeMode) -> Self {
        Self::start(&format!("t3/{name}"), opts(runtime).model("claude-sonnet-4-6")).await
    }

    /// `scripts`: fixture names (without `.ndjson`), comma-separated, one per launch.
    async fn start(scripts: &str, o: Opts) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = scripts.split(',').map(|s| fixtures().join(format!("{}.ndjson", s.trim_end_matches(".ndjson")))).collect();
        let first = std::fs::read_to_string(&paths[0]).unwrap();
        let start: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
        let prompts = start.pointer("/metadata/prompts").and_then(Value::as_array).map(|p| p.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        // Run where the recording ran, so tool paths come out relative as they would have.
        let cwd = scripts.strip_prefix("t3/").map(|n| PathBuf::from(format!("/tmp/claude-replay-{n}"))).unwrap_or_else(|| PathBuf::from("/tmp/roll-claude-fixture"));
        std::fs::create_dir_all(&cwd).unwrap();
        let argv = dir.path().join("argv");
        let mut env = vec![
            ("ROLL_FAKE_CLAUDE".to_string(), paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(",")),
            ("ROLL_FAKE_ARGV".to_string(), argv.to_string_lossy().into_owned()),
        ];
        if o.exit_now {
            env.push(("ROLL_FAKE_EXIT".into(), "now".into()));
        }
        let claude = Claude::new(ProviderConfig { binary: Some(std::env::current_exe().unwrap()), args: vec![], env, auto_compact_window: o.auto_compact_window });
        let cfg = SessionConfig {
            cwd: cwd.clone(),
            model: o.model,
            effort: o.effort,
            options: o.options,
            runtime_mode: o.runtime,
            interaction_mode: o.interaction,
            resume: o.resume,
            resume_at: None,
            fork: o.fork,
            env: o.env,
            mcp: o.mcp,
            instructions: o.instructions,
        };
        let (session, rx) = claude.start(cfg.clone()).await.unwrap();
        Self { claude, cfg, session, rx, events: Vec::new(), prompts, argv, deltas: false, refs: false, context: false, _dir: dir, cwd }
    }

    /// A second session of the same Claude (its launches go on through the scripts), forking
    /// `fork`. The first one must be closed.
    async fn fork(&mut self, fork: ForkFrom) {
        let cfg = SessionConfig { fork: Some(fork), ..self.cfg.clone() };
        (self.session, self.rx) = self.claude.start(cfg).await.unwrap();
    }

    /// A new session of the same Claude, resuming `session` at turn `at` (a rollback). The one
    /// before must be closed.
    async fn resume_at(&mut self, session: &str, at: &str) {
        let cfg = SessionConfig { resume: Some(session.into()), resume_at: Some(at.into()), fork: None, ..self.cfg.clone() };
        (self.session, self.rx) = self.claude.start(cfg).await.unwrap();
    }

    /// The refs of the turns that ended so far, oldest first.
    fn turn_refs(&self) -> Vec<String> {
        self.events.iter().filter_map(|e| if let ProviderEvent::TurnRef { id } = e { Some(id.clone()) } else { None }).collect()
    }

    /// The text of the newest completed message.
    fn last_message(&self) -> String {
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                ProviderEvent::Item(i) if i.status == NodeStatus::Completed => match &i.body {
                    NodeBody::Message { text } => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_default()
    }

    async fn send(&mut self, prompt: usize) {
        let text = self.prompts[prompt].clone();
        self.send_text(&text).await;
    }

    async fn send_text(&mut self, text: &str) {
        self.session.send(TurnInput { text: text.into(), attachments: vec![] }).await.unwrap();
    }

    async fn steer(&mut self, prompt: usize) {
        let text = self.prompts[prompt].clone();
        self.session.steer(TurnInput { text, attachments: vec![] }).await.unwrap();
    }

    async fn respond(&mut self, id: &str, decision: Decision) {
        self.session.respond(id, &Response::Decision { decision, message: None }).await.unwrap();
    }

    async fn until(&mut self, done: impl Fn(&ProviderEvent) -> bool) -> ProviderEvent {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), self.rx.recv()).await.expect("no event in time").unwrap_or_else(|| panic!("events ended: {:#?}", self.events));
            self.events.push(event.clone());
            if let ProviderEvent::Exited { error: Some(e) } = &event
                && !done(&event)
            {
                panic!("the session exited: {e}");
            }
            if done(&event) {
                return event;
            }
        }
    }

    async fn turn(&mut self) -> TurnEnd {
        match self.until(|e| matches!(e, ProviderEvent::TurnEnded(_))).await {
            ProviderEvent::TurnEnded(end) => end,
            _ => unreachable!(),
        }
    }

    async fn request(&mut self) -> (String, RequestBody) {
        match self.until(|e| matches!(e, ProviderEvent::Request { .. })).await {
            ProviderEvent::Request { id, body, .. } => (id, body),
            _ => unreachable!(),
        }
    }

    async fn until_exit(&mut self) {
        self.until(|e| matches!(e, ProviderEvent::Exited { .. })).await;
    }

    async fn close(&mut self) {
        self.session.close().await;
        self.until_exit().await;
    }

    fn with_deltas(&mut self) -> &mut Self {
        self.deltas = true;
        self
    }

    fn with_context(&mut self) -> &mut Self {
        self.context = true;
        self
    }

    fn with_refs(&mut self) -> &mut Self {
        self.refs = true;
        self
    }

    fn rendered(&self) -> Vec<String> {
        let shown = |e: &&ProviderEvent| match e {
            ProviderEvent::TextDelta { .. } => self.deltas,
            ProviderEvent::TurnRef { .. } => self.refs,
            ProviderEvent::Context(_) => self.context,
            _ => true,
        };
        let mut out: Vec<String> = self.events.iter().filter(shown).map(render).collect();
        // The ids the adapter generates for new sessions and forks are random.
        let ours: Vec<String> = self.launches().iter().flatten().filter_map(|a| a.strip_prefix("--session-id=")).map(|id| format!("thread {id}")).collect();
        for line in &mut out {
            if ours.contains(line) {
                *line = "thread (new)".into();
            }
        }
        out
    }

    fn expect(&self, lines: &[&str]) {
        let got = self.rendered();
        if got != lines {
            panic!("events differ.\n--- got:\n{}\n--- want:\n{}", got.join("\n"), lines.join("\n"));
        }
    }

    fn expect_tail(&self, lines: &[&str]) {
        let got = self.rendered();
        let tail = &got[got.len().saturating_sub(lines.len())..];
        assert_eq!(tail, lines, "all events:\n{}", got.join("\n"));
    }

    /// The rendered item events for one provider item id.
    fn items(&self, id: &str) -> Vec<String> {
        self.events.iter().filter(|e| matches!(e, ProviderEvent::Item(i) if i.id == id)).map(render).collect()
    }

    /// Each launch's arguments, as the fake saw them.
    fn launches(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.argv).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.cwd);
    }
}

fn has_flags(args: &[String], flags: &[&str]) -> bool {
    args.windows(flags.len()).any(|w| w.iter().zip(flags).all(|(a, f)| a == f))
}

fn short(text: &str, max: usize) -> String {
    let text = text.replace('\n', "\\n").replace('\t', "\\t");
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

fn render(e: &ProviderEvent) -> String {
    match e {
        ProviderEvent::NativeThread { id } => format!("thread {id}"),
        ProviderEvent::Model { model } => format!("model {model}"),
        ProviderEvent::Item(i) => {
            let status = match i.status {
                NodeStatus::Running => "running",
                NodeStatus::Waiting => "waiting",
                NodeStatus::Completed => "completed",
                NodeStatus::Failed => "failed",
                NodeStatus::Interrupted => "interrupted",
            };
            let parent = i.parent.as_ref().map(|p| format!(" ^{p}")).unwrap_or_default();
            let body = match &i.body {
                NodeBody::Message { text } => format!("message {}", short(text, 60)),
                NodeBody::Reasoning { text } => format!("reasoning {}", short(text, 40)),
                NodeBody::Plan { text } => format!("plan {}", short(text, 60)),
                NodeBody::System { text } => format!("system {text}"),
                NodeBody::Compaction { before_tokens, after_tokens, error } => {
                    let n = |t: &Option<u64>| t.map_or("?".to_string(), |t| t.to_string());
                    let error = error.as_ref().map(|e| format!(" error={e}")).unwrap_or_default();
                    format!("compaction {}→{}{error}", n(before_tokens), n(after_tokens))
                }
                NodeBody::Todos { items } => {
                    let items: Vec<String> = items
                        .iter()
                        .map(|t| {
                            let mark = match t.status {
                                agent_proto::TodoStatus::Completed => "[x]",
                                agent_proto::TodoStatus::InProgress => "[~]",
                                agent_proto::TodoStatus::Pending => "[ ]",
                            };
                            format!("{mark} {}", t.text)
                        })
                        .collect();
                    format!("todos {}", items.join("; "))
                }
                NodeBody::Subagent { agent, prompt, result, progress, .. } => {
                    let result = result.as_ref().map(|r| format!(" result={}", short(r, 40))).unwrap_or_default();
                    let progress = progress.as_ref().map(|p| format!(" progress={}", short(p, 40))).unwrap_or_default();
                    format!("subagent {agent} prompt={}{progress}{result}", short(prompt, 40))
                }
                NodeBody::Tool(t) => {
                    let kind = serde_json::to_value(t.kind).unwrap();
                    let mut s = format!("tool {}/{} \"{}\"", t.name, kind.as_str().unwrap(), short(&t.title, 60));
                    if let Some(code) = t.exit_code {
                        s.push_str(&format!(" exit={code}"));
                    }
                    if let Some(out) = &t.output {
                        s.push_str(&format!(" out={}", short(out, 60)));
                    }
                    if !t.files.is_empty() {
                        let files: Vec<String> = t.files.iter().map(|f| format!("{}({})", f.path, serde_json::to_value(f.change).unwrap().as_str().unwrap())).collect();
                        s.push_str(&format!(" files=[{}]", files.join(", ")));
                    }
                    s
                }
                NodeBody::Request { request } => format!("request node {request}"),
            };
            format!("{status} {}{parent}: {body}", i.id)
        }
        ProviderEvent::TextDelta { item, text } => format!("delta {item} {}", short(text, 30)),
        ProviderEvent::OutputDelta { item, text } => format!("output {item} {}", short(text, 30)),
        ProviderEvent::Request { id, item, body } => {
            let what = match body {
                RequestBody::Approval(a) => {
                    let decisions: Vec<String> = a.decisions.iter().map(|d| serde_json::to_value(d).unwrap().as_str().unwrap().to_string()).collect();
                    format!("{} \"{}\" [{}]", serde_json::to_value(a.action).unwrap().as_str().unwrap(), a.title, decisions.join(", "))
                }
                RequestBody::Questions { questions } => {
                    let qs: Vec<String> = questions
                        .iter()
                        .map(|q| {
                            let options: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
                            format!("[{}: {} ({}){}]", q.header, q.question, options.join(" | "), if q.multi { " multi" } else { "" })
                        })
                        .collect();
                    format!("questions {}", qs.join(" "))
                }
            };
            format!("request {id} for {}: {what}", item.as_deref().unwrap_or("-"))
        }
        ProviderEvent::RequestCancelled { id } => format!("withdrawn {id}"),
        ProviderEvent::TurnRef { id } => format!("ref {id}"),
        ProviderEvent::Usage(u) => format!(
            "usage in={} out={} cached={} cost={}",
            u.input_tokens,
            u.output_tokens,
            u.cached_tokens,
            u.cost_usd.map(|c| format!("{c:.6}")).unwrap_or_else(|| "-".into())
        ),
        ProviderEvent::Context(c) => format!(
            "context {}/{}",
            c.used_tokens,
            c.max_tokens.map(|m| m.to_string()).unwrap_or_else(|| "?".into())
        ),
        ProviderEvent::TurnEnded(TurnEnd::Completed) => "turn completed".into(),
        ProviderEvent::TurnEnded(TurnEnd::Interrupted) => "turn interrupted".into(),
        ProviderEvent::TurnEnded(TurnEnd::Failed(m)) => format!("turn failed: {m}"),
        ProviderEvent::Exited { error: None } => "exited".into(),
        ProviderEvent::Exited { error: Some(e) } => format!("exited: {e}"),
    }
}

// ---------------------------------------------------------------------------------------------
// The fake CLI
// ---------------------------------------------------------------------------------------------

mod fake {
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::io::{BufRead, Write};

    enum Step {
        /// The next line the adapter writes must contain `pattern`. A control request's id is
        /// remembered under `alias` (the transcript's id); `respond` answers it at once.
        Expect { pattern: Value, alias: Option<String>, respond: Option<Value> },
        Emit(Value),
        Stderr(String),
        /// These arguments, in this order, must be on the command line.
        Args(Vec<String>),
        /// These variables must be set to these values, and `CLAUDECODE` unset.
        Env(Vec<(String, String)>),
        Exit(i32),
    }

    pub fn main(scripts: &str) -> ! {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        if argv.iter().any(|a| a == "--version") {
            println!("{}", std::env::var("ROLL_FAKE_VERSION").unwrap_or_else(|_| "2.1.310 (Claude Code)".into()));
            std::process::exit(0);
        }
        if argv.iter().any(|a| a == "--no-session-persistence") {
            probe();
        }
        let mut launch = 0;
        if let Ok(path) = std::env::var("ROLL_FAKE_ARGV") {
            launch = std::fs::read_to_string(&path).map(|s| s.lines().count()).unwrap_or(0);
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
            writeln!(f, "{}", serde_json::to_string(&argv).unwrap()).unwrap();
        }
        let texts: Vec<String> = scripts.split(',').map(|p| std::fs::read_to_string(p).unwrap()).collect();
        let launches: Vec<String> = texts.iter().flat_map(|t| launches(t)).collect();
        let mut text = launches[launch.min(launches.len() - 1)].clone();
        // A fork runs under the session id the adapter gave it (`--session-id` with
        // `--fork-session`), as the real CLI does: the transcript's id for the copy becomes it.
        if let (Some(recorded), Some(ours)) = (texts.iter().find_map(|t| forked_id(t)), our_fork_id()) {
            text = text.replace(&recorded, &ours);
        }
        run(load(&text), &argv)
    }

    /// A transcript split into launches: a T3 transcript ends one at each `runtime_exit`.
    fn launches(text: &str) -> Vec<String> {
        let mut out = vec![String::new()];
        for line in text.lines() {
            out.last_mut().unwrap().push_str(line);
            out.last_mut().unwrap().push('\n');
            if serde_json::from_str::<Value>(line).is_ok_and(|v| v["type"] == "runtime_exit") {
                out.push(String::new());
            }
        }
        out.retain(|l| !l.trim().is_empty());
        out
    }

    /// The session id a T3 transcript's SDK `forkSession` call answered with.
    fn forked_id(text: &str) -> Option<String> {
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["frame"]["type"] == "session.forked")
            .and_then(|v| v["frame"]["sessionId"].as_str().map(str::to_string))
    }

    /// The `--session-id` of the newest launch that forked.
    fn our_fork_id() -> Option<String> {
        let launches = std::fs::read_to_string(std::env::var("ROLL_FAKE_ARGV").ok()?).ok()?;
        launches
            .lines()
            .filter_map(|l| serde_json::from_str::<Vec<String>>(l).ok())
            .rfind(|argv| argv.iter().any(|a| a == "--fork-session"))?
            .iter()
            .find_map(|a| a.strip_prefix("--session-id=").map(str::to_string))
    }

    /// The account check: answer `initialize` with `ROLL_FAKE_ACCOUNT`.
    fn probe() -> ! {
        let account: Value = std::env::var("ROLL_FAKE_ACCOUNT").ok().and_then(|a| serde_json::from_str(&a).ok()).unwrap_or(Value::Null);
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req.pointer("/request/subtype").and_then(Value::as_str), Some("initialize"));
        let id = req["request_id"].clone();
        println!("{}", json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {"account": account}}}));
        drain_and_exit(0)
    }

    fn load(text: &str) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut permissions = 0;
        let mut forking = false;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
            if let Some(env) = v.get("env").and_then(Value::as_object) {
                steps.push(Step::Env(env.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect()));
            } else if let Some(args) = v.get("args").and_then(Value::as_array) {
                steps.push(Step::Args(args.iter().map(|a| a.as_str().unwrap().to_string()).collect()));
            } else if let Some(dir) = v.get("dir").and_then(Value::as_str) {
                raw(&mut steps, dir, v["line"].clone());
            } else {
                t3(&mut steps, &v, &mut permissions, &mut forking);
            }
        }
        steps
    }

    /// A line of a raw stdio recording.
    fn raw(steps: &mut Vec<Step>, dir: &str, mut line: Value) {
        match dir {
            "in" => steps.push(Step::Emit(line)),
            "stderr" => steps.push(Step::Stderr(line.as_str().unwrap_or_default().to_string())),
            "exit" => steps.push(Step::Exit(line.as_i64().unwrap_or(0) as i32)),
            "out" => {
                let alias = match line["type"].as_str() {
                    Some("control_request") => line.as_object_mut().unwrap().remove("request_id").and_then(|id| id.as_str().map(str::to_string)),
                    _ => None,
                };
                if let Some(obj) = line.as_object_mut() {
                    obj.remove("session_id");
                }
                steps.push(Step::Expect { pattern: line, alias, respond: None });
            }
            other => panic!("unknown direction {other}"),
        }
    }

    /// A line of a T3 Agent SDK transcript. The SDK forks a session itself (`session.fork`, then
    /// a query resuming the copy); the CLI does it in the launch, so the fork becomes the flags
    /// of the next `query.open`.
    fn t3(steps: &mut Vec<Step>, v: &Value, permissions: &mut u32, forking: &mut bool) {
        let frame = &v["frame"];
        match (v["type"].as_str(), frame["type"].as_str()) {
            (Some("transcript_start"), _) => {}
            (Some("runtime_exit"), _) => steps.push(Step::Exit(0)),
            (Some("expect_outbound"), Some("query.open")) => {
                let o = &frame["options"];
                if let Some(model) = o["model"].as_str() {
                    steps.push(Step::Args(vec!["--model".into(), model.into()]));
                }
                if let Some(mode) = o["permissionMode"].as_str().filter(|m| ["bypassPermissions", "acceptEdits", "default", "plan"].contains(m)) {
                    steps.push(Step::Args(vec!["--permission-mode".into(), mode.into()]));
                }
                if let Some(resume) = o["resume"].as_str().filter(|_| !std::mem::take(forking)) {
                    steps.push(Step::Args(vec![format!("--resume={resume}")]));
                }
                if let Some(at) = o["resumeSessionAt"].as_str() {
                    steps.push(Step::Args(vec![format!("--resume-session-at={at}")]));
                }
                let pattern = json!({"type": "control_request", "request": {"subtype": "initialize"}});
                steps.push(Step::Expect { pattern, alias: None, respond: Some(json!({})) });
            }
            (Some("expect_outbound"), Some("session.fork")) => {
                *forking = true;
                let from = frame["sessionId"].as_str().unwrap();
                steps.push(Step::Args(vec![format!("--resume={from}"), "--fork-session".into()]));
                if let Some(at) = frame.pointer("/options/upToMessageId").and_then(Value::as_str) {
                    steps.push(Step::Args(vec![format!("--resume-session-at={at}")]));
                }
            }
            (Some("emit_inbound"), Some("session.forked")) => {}
            (Some("expect_outbound"), Some("prompt.offer")) => {
                let mut pattern = frame["message"].clone();
                // T3 stamps its prompts with a uuid of its own; the adapter doesn't.
                pattern.as_object_mut().unwrap().remove("uuid");
                steps.push(Step::Expect { pattern, alias: None, respond: None });
            }
            (Some("expect_outbound"), Some("query.interrupt")) => {
                let pattern = json!({"type": "control_request", "request": {"subtype": "interrupt"}});
                steps.push(Step::Expect { pattern, alias: None, respond: Some(json!({})) });
            }
            (Some("emit_inbound"), Some("permission.request")) => {
                *permissions += 1;
                let o = &frame["options"];
                let mut request = json!({
                    "subtype": "can_use_tool",
                    "tool_name": frame["toolName"],
                    "input": frame["input"],
                    "permission_suggestions": o.get("suggestions").cloned().unwrap_or(json!([])),
                    "tool_use_id": o["toolUseID"],
                });
                for (from, to) in [("description", "description"), ("displayName", "display_name"), ("decisionReason", "decision_reason"), ("title", "title"), ("blockedPath", "blocked_path")] {
                    if let Some(x) = o.get(from) {
                        request[to] = x.clone();
                    }
                }
                steps.push(Step::Emit(json!({"type": "control_request", "request_id": format!("perm_{permissions}"), "request": request})));
            }
            (Some("expect_outbound"), Some("permission.response")) => {
                let mut result = frame["result"].clone();
                // Fields the TS SDK adds itself; the CLI never sees them.
                for k in ["toolUseID", "decisionClassification"] {
                    result.as_object_mut().unwrap().remove(k);
                }
                let pattern = json!({"type": "control_response", "response": {"subtype": "success", "request_id": format!("perm_{permissions}"), "response": result}});
                steps.push(Step::Expect { pattern, alias: None, respond: None });
            }
            (Some("emit_inbound"), Some(_)) => steps.push(Step::Emit(frame.clone())),
            other => panic!("unsupported transcript line {other:?}"),
        }
    }

    fn run(steps: Vec<Step>, argv: &[String]) -> ! {
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        let mut out = std::io::stdout().lock();
        let mut ids: HashMap<String, String> = HashMap::new();
        for step in steps {
            match step {
                Step::Args(want) => {
                    if !argv.windows(want.len()).any(|w| w == want.as_slice()) {
                        fail(&format!("expected arguments {want:?} in {argv:?}"));
                    }
                }
                Step::Env(want) => {
                    for (k, v) in want {
                        if std::env::var(&k).ok().as_deref() != Some(v.as_str()) {
                            fail(&format!("expected {k}={v} in the environment"));
                        }
                    }
                    if std::env::var_os("CLAUDECODE").is_some() {
                        fail("CLAUDECODE is set");
                    }
                }
                Step::Expect { pattern, alias, respond } => {
                    let Some(Ok(line)) = lines.next() else { fail(&format!("stdin closed while expecting {pattern}")) };
                    let got: Value = serde_json::from_str(&line).unwrap_or_else(|_| fail(&format!("not JSON: {line}")));
                    if !contains(&got, &pattern) {
                        fail(&format!("expected {pattern}\n got {got}"));
                    }
                    let id = got.get("request_id").and_then(Value::as_str).map(str::to_string);
                    if let (Some(alias), Some(id)) = (alias, &id) {
                        ids.insert(alias, id.clone());
                    }
                    if let (Some(response), Some(id)) = (respond, id) {
                        writeln!(out, "{}", json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": response}})).unwrap();
                        out.flush().unwrap();
                    }
                }
                Step::Emit(mut v) => {
                    if let Some(id) = v.pointer("/response/request_id").and_then(Value::as_str).and_then(|id| ids.get(id)) {
                        v["response"]["request_id"] = json!(id);
                    }
                    writeln!(out, "{v}").unwrap();
                    out.flush().unwrap();
                }
                Step::Stderr(text) => eprintln!("{text}"),
                Step::Exit(code) => {
                    if std::env::var("ROLL_FAKE_EXIT").as_deref() == Ok("now") {
                        std::process::exit(code);
                    }
                    drop(lines);
                    drain_and_exit(code);
                }
            }
        }
        drain_and_exit(0)
    }

    /// Wait for the adapter to close stdin, like the CLI does at the end of its input.
    fn drain_and_exit(code: i32) -> ! {
        for line in std::io::stdin().lock().lines() {
            if line.is_err() {
                break;
            }
        }
        std::process::exit(code)
    }

    fn fail(why: &str) -> ! {
        eprintln!("fake claude: {why}");
        std::process::exit(3)
    }

    /// `pattern` is a subset of `got`: objects by key, arrays element by element, a `null`
    /// pattern also matches a missing key.
    fn contains(got: &Value, pattern: &Value) -> bool {
        match (pattern, got) {
            (Value::Object(p), Value::Object(g)) => p.iter().all(|(k, pv)| match g.get(k) {
                Some(gv) => contains(gv, pv),
                None => pv.is_null(),
            }),
            (Value::Array(p), Value::Array(g)) => p.len() == g.len() && p.iter().zip(g).all(|(pv, gv)| contains(gv, pv)),
            (Value::Number(p), Value::Number(g)) => p.as_f64() == g.as_f64(),
            (p, g) => p == g,
        }
    }
}

