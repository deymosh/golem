//! `golem eval --agent`: the agent suite (`golem_core::agent`) run against
//! the serving model, once over each API, through the port an agent uses.
//!
//! The tasks, the tools, the grading and the request bodies are the core's;
//! what is here is the loop - post, run the calls, post again - and the
//! clock.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use golem_core::agent::{self, Api, Call, Outcome, Task, Verdict};
use golem_core::json;

use crate::config;
use crate::engine::{self, Base};

/// One turn may take this long: a long first prompt on a slow card.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);

/// `golem eval --agent [anthropic|openai] [--task NAME]`: every task (or the
/// one named) on each API asked for (both when none is), a line per task, a
/// RESULT line per API. 0 when the suite ran, whatever it solved; 1 when the
/// engine could not be asked.
pub fn run(home: &Path, args: &[String], out: &mut dyn Write) -> i32 {
    let mut apis = vec![Api::Anthropic, Api::OpenAi];
    let mut only: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "anthropic" => apis = vec![Api::Anthropic],
            "openai" => apis = vec![Api::OpenAi],
            "--task" => match args.get(i + 1) {
                Some(name) if agent::TASKS.iter().any(|t| t.name == name) => {
                    only = Some(name.clone());
                    i += 1;
                }
                other => {
                    let names: Vec<&str> = agent::TASKS.iter().map(|t| t.name).collect();
                    let _ = writeln!(out, "[FAIL] --task takes one of {}, not {other:?}", names.join(", "));
                    return 1;
                }
            },
            other => {
                let _ = writeln!(out, "[FAIL] --agent takes anthropic, openai or --task NAME, not {other:?}");
                return 1;
            }
        }
        i += 1;
    }
    let Some(base) = Base::from_home(home) else {
        let _ = writeln!(out, "[FAIL] LOCAL_AI_BASE_URL in golem.toml is not a URL golem can reach");
        return 1;
    };
    let model = config::resolved(home, "MODEL_ALIAS").unwrap_or_default();
    match suite(&base, &model, &apis, only.as_deref(), Some(&reports_dir(home)), out) {
        Ok(()) => 0,
        Err(()) => 1,
    }
}

/// Where a missed task's report goes: `logs/agent/`, one file per model,
/// API and task, the newest run's.
pub fn reports_dir(home: &Path) -> PathBuf {
    home.join("logs").join("agent")
}

/// The suite on each of `apis` - every task, or `only` the one named: a line
/// per task and a `RESULT` line per API, to `out`, and for each task it
/// missed a report under `reports` ([`agent::failure_report`]). `Err` when a
/// request got no answer at all, which is the engine, not the model, and
/// leaves nothing to grade; the reason is in `out`.
pub fn suite(
    base: &Base,
    model: &str,
    apis: &[Api],
    only: Option<&str>,
    reports: Option<&Path>,
    out: &mut dyn Write,
) -> Result<(), ()> {
    let tasks: Vec<&Task> = agent::TASKS.iter().filter(|t| only.map_or(true, |name| t.name == name)).collect();
    let _ = writeln!(out, "agent suite: {} tasks on {model}, through port {}", tasks.len(), base.port);
    for &api in apis {
        let _ = writeln!(out);
        let _ = writeln!(out, "{} ({})", api.name(), api.path());
        let mut verdicts = Vec::new();
        for task in tasks.iter().copied() {
            match run_task(base, api, model, task) {
                Ok((verdict, report)) => {
                    let _ = writeln!(
                        out,
                        "  {} {:<26} {:>2} turns {:>2} calls{} {:>5.0}s{}",
                        if verdict.solved { "[ OK ]" } else { "[FAIL]" },
                        verdict.task,
                        verdict.turns,
                        verdict.calls,
                        if verdict.bad_calls > 0 { format!(" ({} bad)", verdict.bad_calls) } else { String::new() },
                        verdict.seconds,
                        if verdict.solved { String::new() } else { format!("  {}", verdict.reason) },
                    );
                    if let (Some(dir), Some(report)) = (reports, report) {
                        let path = dir.join(format!("{model}-{}-{}.txt", api.name(), verdict.task));
                        if std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, report)).is_ok() {
                            let _ = writeln!(out, "         what it did: {}", path.display());
                        }
                    }
                    verdicts.push(verdict);
                }
                Err(e) => {
                    let _ = writeln!(out, "[FAIL] {}: {e}", task.name);
                    return Err(());
                }
            }
        }
        let _ = writeln!(out, "{}", agent::result_line(api, &verdicts));
    }
    Ok(())
}

/// One task, to its end: an answer with no calls, or the turn limit. With
/// the verdict, the failure report when the task was missed.
fn run_task(base: &Base, api: Api, model: &str, task: &Task) -> Result<(Verdict, Option<String>), String> {
    let mut workspace = (task.files)();
    let mut trace: Vec<(Call, Outcome)> = Vec::new();
    let mut cut = false;
    let mut body = agent::first_request(api, model, task);
    let mut verdict = Verdict { task: task.name, ..Verdict::default() };
    let start = Instant::now();
    let mut last_text = String::new();
    while verdict.turns < agent::MAX_TURNS {
        let reply = engine::post_json(base, api.path(), &body.to_json(), TURN_TIMEOUT)
            .ok_or_else(|| format!("no answer on {}", api.path()))?;
        if !reply.ok() {
            return Err(format!("HTTP {}: {}", reply.status, reply.error_message()));
        }
        let answer = json::parse(&reply.body).map_err(|e| format!("the answer is not JSON: {e}"))?;
        let turn = agent::read_turn(api, &answer)?;
        verdict.turns += 1;
        verdict.prompt_tokens += turn.prompt_tokens;
        verdict.output_tokens += turn.output_tokens;
        if !turn.text.trim().is_empty() {
            last_text = turn.text.clone();
        }
        if turn.calls.is_empty() {
            // A turn cut at the token cap is not an answer: say so, rather
            // than grade a half-thought as the model's last word.
            if turn.cut {
                cut = true;
            }
            break;
        }
        let results: Vec<_> = turn
            .calls
            .iter()
            .map(|call| {
                let outcome: Outcome = agent::run(&mut workspace, call, task.tests);
                (call.clone(), outcome)
            })
            .collect();
        verdict.calls += results.len();
        verdict.bad_calls += results.iter().filter(|(_, o)| !o.valid).count();
        agent::continue_request(api, &mut body, &answer, &results);
        trace.extend(results);
    }
    verdict.seconds = start.elapsed().as_secs_f64();
    match (task.grade)(&workspace, &last_text) {
        Ok(()) => verdict.solved = true,
        Err(reason) => verdict.reason = reason,
    }
    if !verdict.solved && cut {
        verdict.reason = format!("{} (its last turn was cut at {} tokens)", verdict.reason, agent::MAX_TOKENS);
    }
    if !verdict.solved && verdict.turns >= agent::MAX_TURNS {
        verdict.reason = format!("{} (stopped at {} turns)", verdict.reason, agent::MAX_TURNS);
    }
    let report = (!verdict.solved).then(|| agent::failure_report(&verdict, &(task.files)(), &workspace, &trace, &last_text));
    Ok((verdict, report))
}
