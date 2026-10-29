//! `golem eval --agent`: the agent suite (`golem_core::agent`) run against
//! the serving model, once over each API, through the port an agent uses.
//!
//! The tasks, the tools, the grading and the request bodies are the core's;
//! what is here is the loop - post, run the calls, post again - and the
//! clock.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use golem_core::agent::{self, Api, Outcome, Task, Verdict};
use golem_core::json;

use crate::config;
use crate::engine::{self, Base};

/// One turn may take this long: a long first prompt on a slow card.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);

/// `golem eval --agent [anthropic|openai]`: every task on each API asked
/// for (both when none is), a line per task, a RESULT line per API. 0 when
/// the suite ran, whatever it solved; 1 when the engine could not be asked.
pub fn run(home: &Path, args: &[String], out: &mut dyn Write) -> i32 {
    let apis: Vec<Api> = match args.iter().find(|a| !a.starts_with("--")).map(String::as_str) {
        Some("anthropic") => vec![Api::Anthropic],
        Some("openai") => vec![Api::OpenAi],
        None => vec![Api::Anthropic, Api::OpenAi],
        Some(other) => {
            let _ = writeln!(out, "[FAIL] --agent takes anthropic or openai, not {other:?}");
            return 1;
        }
    };
    let Some(base) = Base::from_home(home) else {
        let _ = writeln!(out, "[FAIL] LOCAL_AI_BASE_URL in golem.toml is not a URL golem can reach");
        return 1;
    };
    let model = config::resolved(home, "MODEL_ALIAS").unwrap_or_default();
    match suite(&base, &model, &apis, out) {
        Ok(()) => 0,
        Err(()) => 1,
    }
}

/// The suite on each of `apis`: a line per task and a `RESULT` line per API,
/// to `out`. `Err` when a request got no answer at all, which is the engine,
/// not the model, and leaves nothing to grade; the reason is in `out`.
pub fn suite(base: &Base, model: &str, apis: &[Api], out: &mut dyn Write) -> Result<(), ()> {
    let _ = writeln!(out, "agent suite: {} tasks on {model}, through port {}", agent::TASKS.len(), base.port);
    for &api in apis {
        let _ = writeln!(out);
        let _ = writeln!(out, "{} ({})", api.name(), api.path());
        let mut verdicts = Vec::new();
        for task in &agent::TASKS {
            match run_task(base, api, model, task) {
                Ok(verdict) => {
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

/// One task, to its end: an answer with no calls, or the turn limit.
fn run_task(base: &Base, api: Api, model: &str, task: &Task) -> Result<Verdict, String> {
    let mut workspace = (task.files)();
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
    }
    verdict.seconds = start.elapsed().as_secs_f64();
    match (task.grade)(&workspace, &last_text) {
        Ok(()) => verdict.solved = true,
        Err(reason) => verdict.reason = reason,
    }
    if !verdict.solved && verdict.turns >= agent::MAX_TURNS {
        verdict.reason = format!("{} (stopped at {} turns)", verdict.reason, agent::MAX_TURNS);
    }
    Ok(verdict)
}
