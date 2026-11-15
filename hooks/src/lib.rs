//! Cork's agent hooks: the library behind `collide-hook` and, in the
//! free version, the single `cork` program (cork-rs links it in).
//!
//! Cork's agent hooks as one native binary.
//!
//! Same contract as the retired Python hooks it replaced; what changed is
//! the cost of getting there: these fire on every tool call
//! an agent makes, and a Python interpreter spends ~33ms booting before it
//! can decide anything, out of a 200ms gate budget.
//!
//! Usage:
//!   collide-hook gate                    < hook.json     (PreToolUse)
//!   collide-hook report                  < hook.json     (PostToolUse / Stop)
//!   collide-hook pre-commit                              (git pre-commit)
//!   collide-hook land [--test CMD]                       (rebase, test, push; see land.rs)
//!   cork commit --mine [-m MSG] [--dry-run]           (commit only this session's lines; see journal.rs)
//!   collide-hook git-baseline [root]                     (weekly history counts; see gitsignal.rs)
//!   collide-hook path-moves [root]                       (files and folders git saw renamed; see renames.rs)
//!   collide-hook login [--server URL] [--repo ID]        (browser sign-in; see login.rs)
//!   collide-hook watch [--once] [...]                    (the disk watcher)
//!   collide-hook check-credentials [cmd]                 (SessionStart)
//!   collide-hook add-credentials [file]
//!   collide-hook self-update <server>                    (detached; see selfupdate.rs)
//!   cork update                                       (the same, now, past every wait)
//!   cork statusline [on|off]                          (Claude Code's status line; see statusline.rs)
//!   cork migrate                                      (retire a repo's old per-repo setup; see machine.rs)
//!   cork mcp                                          (MCP over stdio for the free version; see mcpbridge.rs)
//!   collide-hook listen <session> [--protocol-only]      (the session's listener and protocol daemon; see protod.rs)
//!   cork audit [--full] [--json] [--since 1h] [-n N]  (what this machine sent; see audit.rs)
//!   cork today [--json]                               (the person's day; see day.rs)
//!   cork pings on|off | cork sound on|off             (desktop notices and the catch sound; see pings.rs)
//!   cork trunk on|off|status | cork branches | cork promote   (trunk mode; see trunk.rs)

pub mod audit;
pub mod back;
pub mod harness;
pub mod hold;
pub mod harnesses;
pub mod ide;
pub mod check;
pub mod journal;
pub mod lines;
pub mod verify;
pub mod prompt;
pub mod config;
pub mod day;
pub mod deps;
pub mod branches;
pub mod fixlines;
pub mod flaky;
pub mod gate;
pub mod land;
pub mod listen;
pub mod login;
pub mod machine;
pub mod mapcheck;
pub mod mcpbridge;
pub mod git;
pub mod origin;
pub mod gitsignal;
pub mod http;
pub mod pre_commit;
pub mod peer;
pub mod pings;
pub mod privacy;
pub mod prbadge;
pub mod reads;
pub mod proto;
pub mod protod;
pub mod apply;
pub mod renames;
pub mod report;
pub mod selfupdate;
pub mod sharedtree;
pub mod touch;
pub mod shell;
pub mod shellrun;
pub mod switches;
pub mod spool;
pub mod statusline;
pub mod transcript;
pub mod trunk;
pub mod wake;
pub mod watch;
pub mod worktrees;

use std::io::Read;

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

/// The whole command line, after the program name. Exits the process.
pub fn run(args: Vec<String>) -> ! {
    // before anything is started in the background (see machine.rs)
    machine::no_inherit_std();
    // every protocol frame this process sends goes in the audit log too
    cork_protocol::ws_client::set_tap(audit::ws_tap);
    let command = args.first().map(String::as_str).unwrap_or("");
    let env = config::env_map();
    // which agent harness is calling; named by the generated config, never sniffed
    harness::set(&harness::of(&args));

    let stdin_data = matches!(command, "gate" | "report").then(read_stdin);
    if let Some(ref data) = stdin_data {
        // the machine's hooks stand down in a repo whose old per-repo hooks
        // still run (never both: every event would be handled twice), and
        // start the local server when a session or a prompt begins
        if let Some(code) = machine::before_event(data, &env) {
            std::process::exit(code);
        }
    }
    let stdin_data = stdin_data.unwrap_or_default();

    // Fail open is the whole contract, so a panic anywhere must still exit 0
    // (or 2 only where the gate deliberately blocks). This is the Rust
    // equivalent of the Python hooks' bare `except`.
    let code = std::panic::catch_unwind(|| match command {
        "gate" => {
            // a whole-file Read of a big mapped file: its outline first (reads.rs);
            // a Read is never a write, so nothing below is for it
            if serde_json::from_str::<serde_json::Value>(&stdin_data)
                .is_ok_and(|input| input.get("tool_name").and_then(serde_json::Value::as_str) == Some("Read"))
            {
                let Some(reason) = reads::outline_first(&stdin_data, &env) else { return 0 };
                let (stdout_text, stderr_text, code) = harness::render_decision(false, &reason);
                if !stdout_text.is_empty() {
                    println!("{stdout_text}");
                }
                if !stderr_text.is_empty() {
                    eprint!("{stderr_text}");
                }
                return code;
            }
            // a message to the other agents: signed by the agent that sends it
            if let Some(rewritten) = machine::message_rewrite(&stdin_data) {
                println!("{rewritten}");
                return 0;
            }
            // `cork commit --mine`: stamped with the session it runs for
            if let Some(rewritten) = machine::commit_rewrite(&stdin_data) {
                // and a push chained after it still lands
                let landed = serde_json::from_str::<serde_json::Value>(&rewritten).ok().and_then(|out| {
                    let mut input: serde_json::Value = serde_json::from_str(&stdin_data).ok()?;
                    input["tool_input"] = out.pointer("/hookSpecificOutput/updatedInput")?.clone();
                    land::rewrite(&input.to_string(), &env)
                });
                println!("{}", landed.unwrap_or(rewritten));
                return 0;
            }
            // a commit in a folder another live session works in takes only
            // this session's files, never the other's uncommitted work
            if let Some(reason) = sharedtree::commit_guard(&stdin_data, &env) {
                statusline::caught(&stdin_data, &env);
                let (stdout_text, stderr_text, code) = harness::render_decision(false, &reason);
                if !stdout_text.is_empty() {
                    println!("{stdout_text}");
                }
                if !stderr_text.is_empty() {
                    eprint!("{stderr_text}");
                }
                return code;
            }
            // a plain push becomes a landing: rebase, test and push in one step
            if let Some(rewritten) = land::rewrite(&stdin_data, &env) {
                println!("{rewritten}");
                return 0;
            }
            let (code, stderr_text, notice) = gate::run_with_notice(&stdin_data, &env);
            // a write stopped: a catch, for the status line
            if code == 2 {
                statusline::caught(&stdin_data, &env);
            }
            let (stdout_text, stderr_text, code) = harness::render_decision(code == 0, &stderr_text);
            if !stdout_text.is_empty() {
                println!("{stdout_text}");
            } else if !notice.is_empty() {
                // an allowed write that calls something a teammate changed:
                // the lines go into the agent's context before the write lands
                gate::emit_notice(&stdin_data, &env, &notice);
            }
            if !stderr_text.is_empty() {
                eprint!("{stderr_text}");
            }
            code
        }
        "report" => report::run(&stdin_data, &env),
        "bill-send" => report::bill_send_command(&args, &env),
        "index" => report::index_command(args.get(1).map(String::as_str).unwrap_or(""), &env),
        "tidy-worktrees" => worktrees::tidy_command(args.get(1).map(String::as_str).unwrap_or(""), &env),
        "started" => report::started_command(&args, &env),
        "verify-tests" => verify::run(
            args.get(1).map(String::as_str).unwrap_or(""),
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""),
            &env,
        ),
        "check" => check::check_command(
            args.get(1).map(String::as_str).unwrap_or(""), args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""), args.get(4).map(String::as_str).unwrap_or("[]"), &env),
        "apply" => apply::run_args(&args[1..], &env),
        "land" => land::run(&args[1..], &env),
        "land-branch" => trunk::land_branch_command(&args[1..], &env),
        "trunk" => trunk::command(&args[1..], &env),
        "branches" => trunk::branches_command(&args[1..], &env),
        "promote" => trunk::promote_command(&args[1..], &env),
        "git-baseline" => gitsignal::baseline_command(args.get(1).map(String::as_str).unwrap_or(""), &env),
        "path-moves" => renames::command(args.get(1).map(String::as_str).unwrap_or(""), &env),
        "login" => login::run(&args[1..], &env),
        "install" => machine::install(&args[1..], &env),
        "harnesses" => harnesses::command(&args[1..], &env),
        "upgrade" => machine::upgrade(&args[1..], &env),
        "add" => machine::add(&env),
        "message" => machine::message(&args[1..], &env),
        "handoff" => machine::handoff(&args[1..], &env),
        "ack" => listen::ack(&args[1..], &env),
        "listen" => listen::run(&args[1..], &env),
        "add-repo-wait" => back::command(&args[1..], &env),
        "proto-ingest" => proto::ingest_command(&args[1..], &env),
        "wake" => wake::command(&args[1..], &env),
        "badge" => machine::badge(&args[1..], &env),
        "pr-badge" => prbadge::command(&args[1..], &env),
        "today" => day::command(&args[1..], &env),
        "pings" => pings::command(&args[1..], &env),
        "sound" => pings::sound_command(&args[1..], &env),
        "reads" => reads::command(&args[1..], &env),
        "ping" => pings::ping_command(&args, &env),
        "mcp" => mcpbridge::run(&env),
        "audit" => audit::command(&args[1..]),
        "agents-md" => machine::agents_md(&env),
        "status" if args.iter().any(|a| a == "--mine") => journal::status_command(&args[1..], &env),
        "status" => machine::status(&env),
        "commit" => journal::commit_command(&args[1..], &env),
        "sync" => machine::sync(&env),
        "uninstall" => machine::uninstall(&env),
        "pre-commit" => {
            let (code, stderr_text) = pre_commit::run(&env);
            if !stderr_text.is_empty() {
                eprint!("{stderr_text}");
            }
            code
        }
        "watch" => match watch::parse_args(&args[1..], &env) {
            Ok(options) => watch::run(options, &env),
            Err(problem) => {
                eprintln!("cork watch: {problem}");
                2
            }
        },
        "check-credentials" => {
            report::check_credentials(args.get(1).map(String::as_str).unwrap_or("python"), &env)
        }
        "add-credentials" => {
            report::add_credentials(args.get(1).map(String::as_str).unwrap_or(""), &env)
        }
        "self-update" => selfupdate::run(args.get(1).map(String::as_str).unwrap_or(""), &env),
        "update" => selfupdate::update_command(&env),
        "rollback" => selfupdate::rollback(&env),
        "statusline" => statusline::command(&args[1..], &env),
        "migrate" => machine::migrate(&env),
        "--version" | "version" => {
            // the hook-artifact version is the one that decides staleness, so
            // print it next to the crate version rather than making anyone
            // guess which number the server is comparing against
            // "hook artifacts vN" is what the updater and the plugin's
            // launcher read; the name before it is the program's
            println!(
                "cork {} (hook artifacts v{})",
                env!("CARGO_PKG_VERSION"),
                report::HOOK_VERSION
            );
            0
        }
        _ if machine::serves_local_pub() => {
            println!("{}", machine::help(&env));
            0
        }
        _ => {
            eprintln!(
                "usage: collide-hook \
<gate|report|pre-commit|watch|check-credentials|add-credentials|--version>"
            );
            0
        }
    })
    .unwrap_or(0);

    std::process::exit(code);
}
