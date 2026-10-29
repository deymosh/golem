//! The agent suite: small coding tasks an agent finishes with tools, graded
//! on what it leaves behind.
//!
//! A task is a workspace of a few files held in memory, a request, and a
//! grader. The model gets the tools an agent has - list, read, write,
//! search, run the tests - and the harness answers every call from the
//! workspace: nothing touches a disk or runs a shell, so a run is the same
//! on every machine and a verdict depends on the model alone. The loop runs
//! over either API, Anthropic's Messages or OpenAI's Chat Completions, through
//! the same port an agent uses.
//!
//! Pure: request bodies, answers read, calls run against the workspace,
//! verdicts. The HTTP is the caller's (`golem::eval`).

use crate::json::{self, Value};

/// The most turns a task may take before it counts as not done.
pub const MAX_TURNS: usize = 12;

/// The longest answer one turn may generate.
pub const MAX_TOKENS: u64 = 4096;

/// The API a run speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Anthropic,
    OpenAi,
}

impl Api {
    pub fn path(self) -> &'static str {
        match self {
            Api::Anthropic => "/v1/messages",
            Api::OpenAi => "/v1/chat/completions",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Api::Anthropic => "anthropic",
            Api::OpenAi => "openai",
        }
    }
}

// ---------------------------------------------------------------
// The workspace
// ---------------------------------------------------------------

/// The files a task starts from and the agent changes, by path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Workspace {
    files: Vec<(String, String)>,
}

impl Workspace {
    pub fn new(files: &[(&str, &str)]) -> Workspace {
        Workspace { files: files.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect() }
    }

    pub fn read(&self, path: &str) -> Option<&str> {
        let path = clean(path);
        self.files.iter().find(|(p, _)| *p == path).map(|(_, t)| t.as_str())
    }

    pub fn write(&mut self, path: &str, text: &str) {
        let path = clean(path);
        match self.files.iter_mut().find(|(p, _)| *p == path) {
            Some((_, t)) => *t = text.to_string(),
            None => self.files.push((path, text.to_string())),
        }
    }

    pub fn paths(&self) -> Vec<&str> {
        let mut paths: Vec<&str> = self.files.iter().map(|(p, _)| p.as_str()).collect();
        paths.sort_unstable();
        paths
    }

    pub fn files(&self) -> &[(String, String)] {
        &self.files
    }
}

/// A path as the workspace keys it: no leading `./` or `/`, forward slashes.
fn clean(path: &str) -> String {
    path.trim().replace('\\', "/").trim_start_matches("./").trim_start_matches('/').to_string()
}

// ---------------------------------------------------------------
// The tools
// ---------------------------------------------------------------

/// One tool: its name, what it does, and its parameters as
/// `(name, description)`, every one a required string.
struct Tool {
    name: &'static str,
    description: &'static str,
    params: &'static [(&'static str, &'static str)],
}

const TOOLS: [Tool; 5] = [
    Tool { name: "list_files", description: "List every file in the project.", params: &[] },
    Tool { name: "read_file", description: "Read a file of the project.", params: &[("path", "The file's path")] },
    Tool {
        name: "write_file",
        description: "Write a file of the project, replacing all of its content.",
        params: &[("path", "The file's path"), ("content", "The file's whole new content")],
    },
    Tool {
        name: "search",
        description: "Find the lines of the project's files that contain a text, as path:line: text.",
        params: &[("text", "The text to look for, matched exactly")],
    },
    Tool { name: "run_tests", description: "Run the project's tests and report the result.", params: &[] },
];

fn schema(tool: &Tool) -> Value {
    Value::object(vec![
        ("type", Value::str("object")),
        (
            "properties",
            Value::Object(
                tool.params
                    .iter()
                    .map(|(name, description)| {
                        (
                            name.to_string(),
                            Value::object(vec![("type", Value::str("string")), ("description", Value::str(description))]),
                        )
                    })
                    .collect(),
            ),
        ),
        ("required", Value::Array(tool.params.iter().map(|(name, _)| Value::str(name)).collect())),
    ])
}

/// The tools in the request's shape.
pub fn tools(api: Api) -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| match api {
            Api::Anthropic => Value::object(vec![
                ("name", Value::str(tool.name)),
                ("description", Value::str(tool.description)),
                ("input_schema", schema(tool)),
            ]),
            Api::OpenAi => Value::object(vec![
                ("type", Value::str("function")),
                (
                    "function",
                    Value::object(vec![
                        ("name", Value::str(tool.name)),
                        ("description", Value::str(tool.description)),
                        ("parameters", schema(tool)),
                    ]),
                ),
            ]),
        })
        .collect()
}

/// A call the model made: its id, the tool, and the arguments as it sent
/// them (a JSON text for OpenAI, an object for Anthropic, both kept as text).
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// What a call did: the text the model reads, and whether the call was
/// well formed (a known tool, arguments that parse, every one it needs).
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub text: String,
    pub valid: bool,
}

/// Run one call against the workspace. `tests` is the task's test run.
pub fn run(workspace: &mut Workspace, call: &Call, tests: fn(&Workspace) -> String) -> Outcome {
    let bad = |text: String| Outcome { text, valid: false };
    let Some(tool) = TOOLS.iter().find(|t| t.name == call.name) else {
        return bad(format!("Error: there is no tool called {:?}.", call.name));
    };
    let args = if call.arguments.trim().is_empty() { Ok(Value::Object(Vec::new())) } else { json::parse(&call.arguments) };
    let Ok(args) = args else {
        return bad(format!("Error: the arguments are not JSON: {}", call.arguments));
    };
    let mut values = Vec::new();
    for (name, _) in tool.params {
        match args.get(name).and_then(Value::as_str) {
            Some(value) => values.push(value.to_string()),
            None => return bad(format!("Error: {} needs a string \"{name}\".", tool.name)),
        }
    }
    let text = match tool.name {
        "list_files" => workspace.paths().join("\n"),
        "read_file" => match workspace.read(&values[0]) {
            Some(text) => text.to_string(),
            None => format!("Error: there is no file {:?}. list_files shows what there is.", values[0]),
        },
        "write_file" => {
            workspace.write(&values[0], &values[1]);
            format!("Wrote {} ({} bytes).", clean(&values[0]), values[1].len())
        }
        "search" => {
            let needle = values[0].as_str();
            let hits: Vec<String> = workspace
                .files
                .iter()
                .flat_map(move |(path, text)| {
                    text.lines()
                        .enumerate()
                        .filter(move |(_, line)| line.contains(needle))
                        .map(move |(n, line)| format!("{path}:{}: {}", n + 1, line.trim()))
                })
                .take(50)
                .collect();
            if hits.is_empty() {
                "No matches.".to_string()
            } else {
                hits.join("\n")
            }
        }
        _ => tests(workspace),
    };
    Outcome { text, valid: true }
}

// ---------------------------------------------------------------
// The tasks
// ---------------------------------------------------------------

/// One task: where it starts, what it asks, and how it is graded.
pub struct Task {
    pub name: &'static str,
    pub prompt: &'static str,
    pub files: fn() -> Workspace,
    /// What `run_tests` answers for the workspace as it stands.
    pub tests: fn(&Workspace) -> String,
    /// Done or not, from the final workspace and the last answer's text.
    pub grade: fn(&Workspace, &str) -> Result<(), String>,
}

const SYSTEM: &str = "You are a coding agent working in a small project. Use the tools to look at the \
project and change it; do not guess what a file says. When the work is done, answer with a short \
summary of what you did.";

const CALC: &str = "def add(a, b):\n    return a - b\n\n\ndef mul(a, b):\n    return a * b\n";
const CALC_TEST: &str = "from calc import add, mul\n\n\ndef test_add():\n    assert add(2, 3) == 5\n\n\ndef test_mul():\n    assert mul(2, 3) == 6\n";

fn fix_files() -> Workspace {
    Workspace::new(&[("calc.py", CALC), ("test_calc.py", CALC_TEST)])
}

fn fix_tests(w: &Workspace) -> String {
    let code = w.read("calc.py").unwrap_or("");
    let add_ok = code.contains("return a + b") || code.contains("return b + a");
    let mul_ok = code.contains("return a * b") || code.contains("return b * a");
    let tests_kept = w.read("test_calc.py") == Some(CALC_TEST);
    match (add_ok && mul_ok, tests_kept) {
        (_, false) => "FAILED: test_calc.py was changed; the tests are not to be edited.".into(),
        (true, true) => "2 passed".into(),
        (false, true) => {
            let mut out = String::new();
            if !add_ok {
                out.push_str("FAILED test_calc.py::test_add - assert add(2, 3) == 5\n");
            }
            if !mul_ok {
                out.push_str("FAILED test_calc.py::test_mul - assert mul(2, 3) == 6\n");
            }
            out.push_str(&format!("{} failed, {} passed", 2 - add_ok as u8 - mul_ok as u8, add_ok as u8 + mul_ok as u8));
            out
        }
    }
}

fn fix_grade(w: &Workspace, _: &str) -> Result<(), String> {
    match fix_tests(w).as_str() {
        "2 passed" => Ok(()),
        other => Err(other.lines().next().unwrap_or("").to_string()),
    }
}

const USERS: &str = "def get_user(user_id):\n    return {\"id\": user_id}\n";
const API: &str = "from users import get_user\n\n\ndef handler(request):\n    return get_user(request[\"id\"])\n";
const JOBS: &str = "from users import get_user\n\n\ndef nightly(ids):\n    return [get_user(i) for i in ids]\n";

fn rename_files() -> Workspace {
    Workspace::new(&[("users.py", USERS), ("api.py", API), ("jobs.py", JOBS), ("README.md", "# Service\n")])
}

fn rename_grade(w: &Workspace, _: &str) -> Result<(), String> {
    for (path, text) in w.files() {
        if text.contains("get_user") {
            return Err(format!("{path} still says get_user"));
        }
    }
    for path in ["users.py", "api.py", "jobs.py"] {
        let text = w.read(path).unwrap_or("");
        if !text.contains("fetch_user") {
            return Err(format!("{path} does not say fetch_user"));
        }
    }
    if !w.read("users.py").unwrap_or("").contains("def fetch_user(") {
        return Err("users.py does not define fetch_user".into());
    }
    Ok(())
}

fn no_tests(_: &Workspace) -> String {
    "No tests in this project.".into()
}

/// A long settings file with one line that matters, the same every run.
fn config_text() -> String {
    let mut text = String::from("# service settings\n");
    for i in 0..1500u32 {
        // A fixed scramble, so the values look like settings and not a count.
        let value = (i * 7919 + 13) % 10007;
        text.push_str(&format!("setting_{i:04} = {value}\n"));
        if i == 1111 {
            text.push_str("max_connections = 4711\n");
        }
    }
    text
}

fn find_files() -> Workspace {
    let mut w = Workspace::new(&[("README.md", "# Service\nSettings live in config/service.ini.\n")]);
    w.write("config/service.ini", &config_text());
    w
}

fn find_grade(_: &Workspace, answer: &str) -> Result<(), String> {
    if answer.contains("4711") {
        Ok(())
    } else {
        Err("the answer does not give 4711".into())
    }
}

fn recover_files() -> Workspace {
    Workspace::new(&[("docs/notes.txt", "release on friday\nship the fix\n")])
}

fn recover_grade(w: &Workspace, _: &str) -> Result<(), String> {
    match w.read("summary.txt") {
        Some(text) if text.trim().eq_ignore_ascii_case("release on friday") => Ok(()),
        Some(text) => Err(format!("summary.txt says {:?}", text.trim())),
        None => Err("no summary.txt".into()),
    }
}

/// The suite, in the order it runs.
pub const TASKS: [Task; 4] = [
    Task {
        name: "fix-a-failing-test",
        prompt: "The tests in this project fail. Find out why and fix the code, not the tests. Run the tests to check.",
        files: fix_files,
        tests: fix_tests,
        grade: fix_grade,
    },
    Task {
        name: "rename-across-files",
        prompt: "Rename the function get_user to fetch_user everywhere in the project, its definition and every use.",
        files: rename_files,
        tests: no_tests,
        grade: rename_grade,
    },
    Task {
        name: "find-in-a-long-file",
        prompt: "What is max_connections set to in this service's settings? Answer with the number.",
        files: find_files,
        tests: no_tests,
        grade: find_grade,
    },
    Task {
        name: "recover-from-a-wrong-path",
        prompt: "Read notes.txt and write its first line, exactly, to summary.txt.",
        files: recover_files,
        tests: no_tests,
        grade: recover_grade,
    },
];

// ---------------------------------------------------------------
// The conversation
// ---------------------------------------------------------------

/// The first request of a task.
pub fn first_request(api: Api, model: &str, task: &Task) -> Value {
    let user = Value::object(vec![("role", Value::str("user")), ("content", Value::str(task.prompt))]);
    let mut members = vec![
        ("model", Value::str(model)),
        ("max_tokens", Value::int(MAX_TOKENS)),
        ("temperature", Value::int(0)),
        ("tools", Value::Array(tools(api))),
    ];
    match api {
        Api::Anthropic => {
            members.push(("system", Value::str(SYSTEM)));
            members.push(("messages", Value::Array(vec![user])));
        }
        Api::OpenAi => {
            let system = Value::object(vec![("role", Value::str("system")), ("content", Value::str(SYSTEM))]);
            members.push(("messages", Value::Array(vec![system, user])));
        }
    }
    Value::object(members)
}

/// One answer read: its text, its calls, and the tokens it counted.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<Call>,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
}

fn count(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_f64).map_or(0, |n| n.max(0.0) as u64)
}

/// Read an answer, or say why it cannot be read.
pub fn read_turn(api: Api, answer: &Value) -> Result<Turn, String> {
    match api {
        Api::Anthropic => {
            let blocks = answer.get("content").and_then(Value::as_array).ok_or("the answer has no content")?;
            let mut turn = Turn::default();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => turn.text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("tool_use") => turn.calls.push(Call {
                        id: block.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                        name: block.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        arguments: block.get("input").map(Value::to_json).unwrap_or_default(),
                    }),
                    _ => {}
                }
            }
            let usage = answer.get("usage");
            turn.prompt_tokens = count(usage.and_then(|u| u.get("input_tokens")));
            turn.output_tokens = count(usage.and_then(|u| u.get("output_tokens")));
            Ok(turn)
        }
        Api::OpenAi => {
            let message = answer
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .ok_or("the answer has no message")?;
            let mut turn = Turn {
                text: message.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
                ..Turn::default()
            };
            for call in message.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                let function = call.get("function");
                turn.calls.push(Call {
                    id: call.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                    name: function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("").to_string(),
                    arguments: function.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("").to_string(),
                });
            }
            let usage = answer.get("usage");
            turn.prompt_tokens = count(usage.and_then(|u| u.get("prompt_tokens")));
            turn.output_tokens = count(usage.and_then(|u| u.get("completion_tokens")));
            Ok(turn)
        }
    }
}

/// The next request: the answer that made the calls, and their results.
pub fn continue_request(api: Api, body: &mut Value, answer: &Value, results: &[(Call, Outcome)]) {
    let mut messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    match api {
        Api::Anthropic => {
            let content = answer.get("content").cloned().unwrap_or(Value::Array(Vec::new()));
            messages.push(Value::object(vec![("role", Value::str("assistant")), ("content", content)]));
            let blocks = results
                .iter()
                .map(|(call, outcome)| {
                    Value::object(vec![
                        ("type", Value::str("tool_result")),
                        ("tool_use_id", Value::str(&call.id)),
                        ("content", Value::str(&outcome.text)),
                    ])
                })
                .collect();
            messages.push(Value::object(vec![("role", Value::str("user")), ("content", Value::Array(blocks))]));
        }
        Api::OpenAi => {
            let message = answer
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .cloned()
                .unwrap_or(Value::Null);
            messages.push(message);
            for (call, outcome) in results {
                messages.push(Value::object(vec![
                    ("role", Value::str("tool")),
                    ("tool_call_id", Value::str(&call.id)),
                    ("content", Value::str(&outcome.text)),
                ]));
            }
        }
    }
    body.set("messages", Value::Array(messages));
}

/// How one task went.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Verdict {
    pub task: &'static str,
    pub solved: bool,
    /// Why not, when it was not.
    pub reason: String,
    pub turns: usize,
    pub calls: usize,
    /// Calls to an unknown tool, or with arguments that did not parse or
    /// lacked one the tool needs.
    pub bad_calls: usize,
    pub seconds: f64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
}

/// The run's summary line, as `eval` prints it and the recorder reads it.
pub fn result_line(api: Api, verdicts: &[Verdict]) -> String {
    let solved = verdicts.iter().filter(|v| v.solved).count();
    let calls: usize = verdicts.iter().map(|v| v.calls).sum();
    let bad: usize = verdicts.iter().map(|v| v.bad_calls).sum();
    let seconds: f64 = verdicts.iter().map(|v| v.seconds).sum();
    let turns: usize = verdicts.iter().map(|v| v.turns).sum();
    format!(
        "RESULT agent_{}={solved}/{} agent_turns={turns} agent_calls={calls} agent_bad_calls={bad} agent_seconds={seconds:.0}",
        api.name(),
        verdicts.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> Call {
        Call { id: "c1".into(), name: name.into(), arguments: arguments.into() }
    }

    #[test]
    fn the_tools_answer_from_the_workspace() {
        let mut w = fix_files();
        let listed = run(&mut w, &call("list_files", ""), fix_tests);
        assert_eq!(listed, Outcome { text: "calc.py\ntest_calc.py".into(), valid: true });
        assert_eq!(run(&mut w, &call("read_file", r#"{"path":"./calc.py"}"#), fix_tests).text, CALC);
        let found = run(&mut w, &call("search", r#"{"text":"return a"}"#), fix_tests).text;
        assert_eq!(found, "calc.py:2: return a - b\ncalc.py:6: return a * b");
        assert!(run(&mut w, &call("run_tests", "{}"), fix_tests).text.ends_with("1 failed, 1 passed"));
        let fixed = Value::object(vec![("path", Value::str("calc.py")), ("content", Value::str(&CALC.replace("a - b", "a + b")))]);
        run(&mut w, &call("write_file", &fixed.to_json()), fix_tests);
        assert_eq!(run(&mut w, &call("run_tests", ""), fix_tests).text, "2 passed");
        assert_eq!(fix_grade(&w, ""), Ok(()));
    }

    #[test]
    fn a_bad_call_is_answered_and_counted() {
        let mut w = fix_files();
        assert!(!run(&mut w, &call("rm", "{}"), fix_tests).valid);
        assert!(!run(&mut w, &call("read_file", "{path"), fix_tests).valid);
        let missing = run(&mut w, &call("read_file", "{}"), fix_tests);
        assert_eq!(missing, Outcome { text: "Error: read_file needs a string \"path\".".into(), valid: false });
        // A file that is not there is a valid call with an error to read.
        let absent = run(&mut w, &call("read_file", r#"{"path":"notes.txt"}"#), fix_tests);
        assert!(absent.valid && absent.text.starts_with("Error: there is no file"));
    }

    #[test]
    fn editing_the_tests_does_not_pass() {
        let mut w = fix_files();
        w.write("test_calc.py", "def test_nothing():\n    pass\n");
        assert!(fix_grade(&w, "").is_err());
    }

    #[test]
    fn each_task_fails_untouched_and_passes_done() {
        let mut fix = fix_files();
        assert!(fix_grade(&fix, "").is_err());
        fix.write("calc.py", &CALC.replace("a - b", "b + a"));
        assert_eq!(fix_grade(&fix, ""), Ok(()));

        let mut rename = rename_files();
        assert_eq!(rename_grade(&rename, ""), Err("users.py still says get_user".into()));
        for path in ["users.py", "api.py", "jobs.py"] {
            let text = rename.read(path).unwrap().replace("get_user", "fetch_user");
            rename.write(path, &text);
        }
        assert_eq!(rename_grade(&rename, ""), Ok(()));

        let find = find_files();
        assert!(find.read("config/service.ini").unwrap().contains("\nmax_connections = 4711\n"));
        assert!(find.read("config/service.ini").unwrap().lines().count() > 1500);
        assert!(find_grade(&find, "It is 4711.").is_ok() && find_grade(&find, "4700").is_err());

        let mut recover = recover_files();
        assert!(recover_grade(&recover, "").is_err());
        recover.write("summary.txt", "release on friday\n");
        assert_eq!(recover_grade(&recover, ""), Ok(()));
    }

    #[test]
    fn requests_and_answers_round_trip_on_both_apis() {
        let task = &TASKS[0];
        let anthropic = first_request(Api::Anthropic, "m", task);
        assert!(anthropic.to_json().contains(r#""system":"You are a coding agent"#));
        assert_eq!(anthropic.get("tools").and_then(Value::as_array).map(Vec::len), Some(5));
        let openai = first_request(Api::OpenAi, "m", task);
        assert_eq!(openai.get("messages").and_then(Value::as_array).map(Vec::len), Some(2));

        let answer = json::parse(
            r#"{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"read_file","input":{"path":"calc.py"}}],"usage":{"input_tokens":900,"output_tokens":20}}"#,
        )
        .unwrap();
        let turn = read_turn(Api::Anthropic, &answer).unwrap();
        assert_eq!((turn.text.as_str(), turn.calls.len(), turn.prompt_tokens), ("Looking.", 1, 900));
        assert_eq!(turn.calls[0].arguments, r#"{"path":"calc.py"}"#);
        let mut body = anthropic.clone();
        let outcome = Outcome { text: CALC.into(), valid: true };
        continue_request(Api::Anthropic, &mut body, &answer, &[(turn.calls[0].clone(), outcome.clone())]);
        let messages = body.get("messages").and_then(Value::as_array).unwrap();
        assert_eq!(messages.len(), 3);
        assert!(messages[2].to_json().contains(r#""tool_use_id":"t1""#));

        let answer = json::parse(
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"x","type":"function","function":{"name":"run_tests","arguments":"{}"}}]}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        )
        .unwrap();
        let turn = read_turn(Api::OpenAi, &answer).unwrap();
        assert_eq!((turn.calls[0].name.as_str(), turn.output_tokens), ("run_tests", 2));
        let mut body = openai.clone();
        continue_request(Api::OpenAi, &mut body, &answer, &[(turn.calls[0].clone(), outcome)]);
        assert_eq!(body.get("messages").and_then(Value::as_array).map(Vec::len), Some(4));
    }

    #[test]
    fn the_result_line_sums_the_run() {
        let v = |solved, calls, bad| Verdict { task: "t", solved, calls, bad_calls: bad, turns: 3, seconds: 10.4, ..Verdict::default() };
        assert_eq!(
            result_line(Api::OpenAi, &[v(true, 4, 0), v(false, 6, 1)]),
            "RESULT agent_openai=1/2 agent_turns=6 agent_calls=10 agent_bad_calls=1 agent_seconds=21"
        );
    }
}
