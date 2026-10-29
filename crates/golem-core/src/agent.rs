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

/// The longest answer one turn may generate, its reasoning included. A
/// thinking model spends thousands of tokens before a call: at 4096 the 35B
/// was cut off mid-thought on the dispatch rename, 2026-10-29, after reading
/// every file it needed, and the cut turn read as a finished one.
pub const MAX_TOKENS: u64 = 16384;

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

const TOOLS: [Tool; 6] = [
    Tool { name: "list_files", description: "List every file in the project.", params: &[] },
    Tool { name: "read_file", description: "Read a file of the project.", params: &[("path", "The file's path")] },
    Tool {
        name: "write_file",
        description: "Write a file of the project, replacing all of its content.",
        params: &[("path", "The file's path"), ("content", "The file's whole new content")],
    },
    Tool {
        name: "edit_file",
        description: "Replace one exact piece of a file with new text. The old text must appear in the file exactly once.",
        params: &[
            ("path", "The file's path"),
            ("old_text", "The exact text to replace, long enough to be unique in the file"),
            ("new_text", "The text to put in its place"),
        ],
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
        "edit_file" => match workspace.read(&values[0]).map(|text| (text.matches(values[1].as_str()).count(), text)) {
            None => format!("Error: there is no file {:?}. list_files shows what there is.", values[0]),
            Some(_) if values[1].is_empty() => "Error: old_text is empty.".to_string(),
            Some((0, _)) => format!("Error: old_text does not appear in {}. Read the file and copy the text exactly.", clean(&values[0])),
            Some((1, text)) => {
                let edited = text.replacen(values[1].as_str(), &values[2], 1);
                workspace.write(&values[0], &edited);
                format!("Edited {}.", clean(&values[0]))
            }
            Some((n, _)) => format!("Error: old_text appears {n} times in {}; include more of the text around it.", clean(&values[0])),
        },
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

// A bug whose cause is in one file and whose obvious fix breaks another: the
// rates are whole percentages because the export prints them, so the fix is
// the division the invoice forgot - and a model that changes the rate instead
// sees the export's test fail and has to run the tests again.
const RATES: &str = "# Rates are whole percentages: 21 means 21%.\n\
# The accounting export prints these numbers as they are.\n\
VAT = 21\nDISCOUNT = 10\n";
const INVOICE: &str = "from shop import rates\n\n\ndef total(net):\n    \
\"\"\"Net, less the discount, plus VAT, to the cent.\"\"\"\n    \
discounted = net * (1 - rates.DISCOUNT / 100)\n    \
return round(discounted * (1 + rates.VAT), 2)\n";
const EXPORT: &str = "from shop import rates\n\n\ndef vat_line():\n    return f\"VAT;{rates.VAT}\"\n";
const SHOP_TEST: &str = "from shop.export import vat_line\nfrom shop.invoice import total\n\n\n\
def test_total():\n    assert total(100) == 108.9\n\n\n\
def test_export():\n    assert vat_line() == \"VAT;21\"\n";

fn shop_files() -> Workspace {
    Workspace::new(&[
        ("shop/rates.py", RATES),
        ("shop/invoice.py", INVOICE),
        ("shop/export.py", EXPORT),
        ("tests/test_shop.py", SHOP_TEST),
    ])
}

/// `NAME = value` in a Python file, the value as written.
fn assigned<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == name).then(|| value.split('#').next().unwrap_or("").trim())
    })
}

/// A float as Python prints one: `1980.0`, `108.9`.
fn python_float(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn shop_tests(w: &Workspace) -> String {
    if w.read("tests/test_shop.py") != Some(SHOP_TEST) {
        return "FAILED: tests/test_shop.py was changed; the tests are not to be edited.".into();
    }
    let rates = w.read("shop/rates.py").unwrap_or("");
    let vat_text = assigned(rates, "VAT").unwrap_or("");
    let mut failed = Vec::new();

    // The invoice, evaluated for the forms a fix takes: the rate divided by
    // 100 where it is used, or used whole.
    let invoice: String = w.read("shop/invoice.py").unwrap_or("").split_whitespace().collect();
    let vat: Option<f64> = vat_text.parse().ok();
    let discount: Option<f64> = assigned(rates, "DISCOUNT").and_then(|d| d.parse().ok());
    let as_fraction = |name: &str| ["/100", "/100.0", "*0.01"].iter().any(|d| invoice.contains(&format!("rates.{name}{d}")));
    let vat_part = if as_fraction("VAT") {
        vat.map(|v| v / 100.0)
    } else if invoice.contains("(1+rates.VAT)") {
        vat
    } else {
        None
    };
    let discount_part = if as_fraction("DISCOUNT") { discount.map(|d| d / 100.0) } else { None };
    match (vat_part, discount_part) {
        (Some(v), Some(d)) => {
            let total = (100.0 * (1.0 - d) * (1.0 + v) * 100.0).round() / 100.0;
            if (total - 108.9).abs() > 0.004 {
                failed.push(format!("FAILED tests/test_shop.py::test_total - assert {} == 108.9", python_float(total)));
            }
        }
        _ => failed.push("FAILED tests/test_shop.py::test_total - AssertionError: assert total(100) == 108.9".into()),
    }

    let export_line = format!("VAT;{vat_text}");
    if w.read("shop/export.py") != Some(EXPORT) {
        failed.push("FAILED tests/test_shop.py::test_export - the export's format changed".into());
    } else if export_line != "VAT;21" {
        failed.push(format!("FAILED tests/test_shop.py::test_export - AssertionError: assert '{export_line}' == 'VAT;21'"));
    }
    if failed.is_empty() {
        return "2 passed".into();
    }
    format!("{}\n{} failed, {} passed", failed.join("\n"), failed.len(), 2 - failed.len())
}

fn shop_grade(w: &Workspace, _: &str) -> Result<(), String> {
    match shop_tests(w).as_str() {
        "2 passed" => Ok(()),
        other => Err(other.lines().next().unwrap_or("").to_string()),
    }
}

// A setting whose value is in the last of a chain of files: the long file
// names it first, the files it includes override it, and a profile nothing
// includes says something else again.
fn includes_files() -> Workspace {
    let mut w = Workspace::new(&[(
        "README.md",
        "# Service\nSettings are read from config/service.ini, then from each file its include line \
names, in that order. A file read later overrides one read earlier.\n",
    )]);
    w.write("config/service.ini", &format!("{}include = profiles/base.ini, profiles/prod.ini\n", config_text()));
    w.write("config/profiles/base.ini", "# defaults for every environment\nmax_connections = 2048\ntimeout = 30\n");
    w.write("config/profiles/prod.ini", "# production\ntimeout = 60\nmax_connections = 6000\n");
    w.write("config/profiles/staging.ini", "# staging\nmax_connections = 8000\ntimeout = 10\n");
    w
}

fn includes_grade(_: &Workspace, answer: &str) -> Result<(), String> {
    if answer.contains("6000") {
        Ok(())
    } else {
        Err("the answer does not give 6000".into())
    }
}

// A translation to complete: the missing keys added, every existing one kept,
// the placeholders carried over - and the file still JSON.
const EN_JSON: &str = r#"{
  "app": {"title": "Notes", "save": "Save", "cancel": "Cancel", "delete": "Delete"},
  "inbox": {"empty": "No messages", "unread": "You have {count} unread messages", "from": "From {name}"},
  "settings": {"language": "Language", "theme": "Theme", "dark": "Dark", "light": "Light"}
}
"#;
const ES_JSON: &str = r#"{
  "app": {"title": "Notas", "save": "Guardar", "cancel": "Cancelar"},
  "inbox": {"empty": "No hay mensajes"},
  "settings": {"language": "Idioma", "theme": "Tema", "light": "Claro"}
}
"#;

fn locale_files() -> Workspace {
    Workspace::new(&[("locales/en.json", EN_JSON), ("locales/es.json", ES_JSON)])
}

/// Every `section.key` of a two-level locale file, with its text.
fn entries(value: &Value) -> Vec<(String, Option<String>)> {
    let Value::Object(sections) = value else { return Vec::new() };
    let mut out = Vec::new();
    for (section, keys) in sections {
        let Value::Object(keys) = keys else {
            out.push((section.clone(), None));
            continue;
        };
        for (key, text) in keys {
            out.push((format!("{section}.{key}"), text.as_str().map(str::to_string)));
        }
    }
    out
}

fn locale_grade(w: &Workspace, _: &str) -> Result<(), String> {
    let es = json::parse(w.read("locales/es.json").unwrap_or("")).map_err(|e| format!("es.json is not JSON: {e}"))?;
    let es = entries(&es);
    let en = entries(&json::parse(EN_JSON).expect("the English file parses"));
    let before = entries(&json::parse(ES_JSON).expect("the Spanish file parses"));
    let text_of = |list: &[(String, Option<String>)], key: &str| list.iter().find(|(k, _)| k == key).and_then(|(_, t)| t.clone());
    for (key, english) in &en {
        let english = english.as_deref().unwrap_or("");
        let Some(spanish) = text_of(&es, key).filter(|t| !t.trim().is_empty()) else {
            return Err(format!("es.json has no {key}"));
        };
        if let Some(kept) = text_of(&before, key) {
            if spanish != kept {
                return Err(format!("es.json changed {key} from {kept:?} to {spanish:?}"));
            }
        } else if spanish == english {
            return Err(format!("es.json's {key} is still English"));
        }
        for placeholder in ["{count}", "{name}"] {
            if english.contains(placeholder) && !spanish.contains(placeholder) {
                return Err(format!("es.json's {key} lost {placeholder}"));
            }
        }
    }
    if let Some((key, _)) = es.iter().find(|(k, _)| text_of(&en, k).is_none()) {
        return Err(format!("es.json has {key}, which en.json does not"));
    }
    Ok(())
}

// A one-character fix in a file too long to rewrite: the edit has to land on
// the one line and leave the other twelve hundred as they were.
const CHECKSUM_AT: u32 = 200;
const CHECKSUM_BUG: &str = "    return sum(data) % 255\n";
const TABLES_TEST: &str = "from tables import checksum, scale_0150\n\n\n\
def test_checksum():\n    assert checksum(bytes(range(256))) == 128\n\n\n\
def test_scale():\n    assert scale_0150(100) == 101.5\n";

fn tables_text() -> String {
    let mut text = String::from("\"\"\"Scaling tables for the meter firmware.\"\"\"\n\n\n");
    for i in 1..=300u32 {
        if i == CHECKSUM_AT {
            text.push_str("def checksum(data):\n    \"\"\"The sum of the bytes, modulo 256.\"\"\"\n");
            text.push_str(CHECKSUM_BUG);
            text.push_str("\n\n");
        }
        text.push_str(&format!("def scale_{i:04}(x):\n    return x * 1.{i:04}\n\n\n"));
    }
    text
}

fn tables_files() -> Workspace {
    Workspace::new(&[("tables.py", &tables_text()), ("test_tables.py", TABLES_TEST)])
}

fn tables_tests(w: &Workspace) -> String {
    if w.read("test_tables.py") != Some(TABLES_TEST) {
        return "FAILED: test_tables.py was changed; the tests are not to be edited.".into();
    }
    let original = tables_text();
    let now = w.read("tables.py").unwrap_or("");
    let (before, after): (Vec<&str>, Vec<&str>) = (original.lines().collect(), now.lines().collect());
    let changed: Vec<usize> = (0..before.len().max(after.len())).filter(|&i| before.get(i) != after.get(i)).collect();
    let bug = before.iter().position(|line| *line == CHECKSUM_BUG.trim_end()).expect("the bug is in the file");
    if before.len() != after.len() || changed.iter().any(|&i| i != bug) {
        return "FAILED test_tables.py::test_scale - tables.py changed outside checksum\n\
                FAILED test_tables.py::test_checksum - tables.py changed outside checksum\n2 failed"
            .into();
    }
    let line: String = after[bug].split_whitespace().collect();
    if ["returnsum(data)%256", "returnsum(data)&255", "returnsum(data)&0xff", "returnsum(data)&0xFF"].contains(&line.as_str()) {
        "2 passed".into()
    } else {
        "FAILED test_tables.py::test_checksum - assert 0 == 128\n1 failed, 1 passed".into()
    }
}

fn tables_grade(w: &Workspace, _: &str) -> Result<(), String> {
    match tables_tests(w).as_str() {
        "2 passed" => Ok(()),
        other => Err(other.lines().next().unwrap_or("").to_string()),
    }
}

// A total over a table, to the cent: no tool adds the numbers up, so the
// model reads sixty rows, keeps the ones the question asks for, and sums.
const ORDERS_HEAD: &str = "id,date,region,amount,status\n";

/// The orders, the same every run: (id, date, region, cents, status).
fn orders() -> Vec<(u32, String, &'static str, u64, &'static str)> {
    let regions = ["EU", "US", "APAC", "EU", "US"];
    let mut seed: u64 = 20_261_029;
    (1..=60u32)
        .map(|id| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let r = seed >> 33;
            let month = 2 + (r % 3) as u32;
            let day = 1 + (r / 3 % 28) as u32;
            let cents = 10_000 + r / 97 % 90_000;
            let status = if r % 7 == 0 { "refunded" } else { "paid" };
            (id, format!("2026-{month:02}-{day:02}"), regions[(r / 11 % 5) as usize], cents, status)
        })
        .collect()
}

fn orders_files() -> Workspace {
    let mut csv = String::from(ORDERS_HEAD);
    for (id, date, region, cents, status) in orders() {
        csv.push_str(&format!("{id},{date},{region},{}.{:02},{status}\n", cents / 100, cents % 100));
    }
    let mut w = Workspace::new(&[("README.md", "# Shop\nOrders are exported to data/orders.csv. Amounts are in euros.\n")]);
    w.write("data/orders.csv", &csv);
    w
}

/// The answer the question has: EU, March 2026, refunds left out.
fn orders_total() -> String {
    let cents: u64 = orders()
        .iter()
        .filter(|(_, date, region, _, status)| *region == "EU" && date.starts_with("2026-03") && *status == "paid")
        .map(|(_, _, _, cents, _)| cents)
        .sum();
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn orders_grade(_: &Workspace, answer: &str) -> Result<(), String> {
    // "1,234.56" and "1 234.56" are the same number.
    let plain: String = answer.chars().filter(|c| *c != ',' && *c != ' ' && *c != '\u{a0}').collect();
    let total = orders_total();
    if plain.contains(&total) {
        Ok(())
    } else {
        Err(format!("the answer does not give {total}"))
    }
}

// A rename a text search does not finish: one caller builds the name from
// pieces, so renaming every `get_user` still breaks it, and only the tests
// say so.
const DISPATCH_USERS: &str = "def get_user(user_id):\n    return {\"id\": user_id}\n\n\ndef get_team(team_id):\n    return {\"team\": team_id}\n";
const DISPATCH_JOBS: &str = "import users\n\n\ndef load(kind, key):\n    \"\"\"One record of any kind, by the users module's lookup for that kind.\"\"\"\n    return getattr(users, \"get_\" + kind)(key)\n";
const DISPATCH_API: &str = "from users import get_user\n\n\ndef handler(request):\n    return get_user(request[\"id\"])\n";
const DISPATCH_TEST: &str = "import api\nimport jobs\n\n\ndef test_handler():\n    assert api.handler({\"id\": 7}) == {\"id\": 7}\n\n\n\
def test_load():\n    assert jobs.load(\"user\", 7) == {\"id\": 7}\n    assert jobs.load(\"team\", 2) == {\"team\": 2}\n";

fn dispatch_files() -> Workspace {
    Workspace::new(&[
        ("users.py", DISPATCH_USERS),
        ("jobs.py", DISPATCH_JOBS),
        ("api.py", DISPATCH_API),
        ("test_app.py", DISPATCH_TEST),
    ])
}

/// Whether `users.py` defines `name`, as a function or an assignment.
fn defines(users: &str, name: &str) -> bool {
    users.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with(&format!("def {name}(")) || line.split('=').next().map(str::trim) == Some(name)
    })
}

fn dispatch_tests(w: &Workspace) -> String {
    if w.read("test_app.py") != Some(DISPATCH_TEST) {
        return "FAILED: test_app.py was changed; the tests are not to be edited.".into();
    }
    let users = w.read("users.py").unwrap_or("");
    let flat = |path: &str| -> String { w.read(path).unwrap_or("").split_whitespace().collect() };
    let mut failed = Vec::new();

    // api.py: whatever it imports from users has to be there, and is what
    // the handler calls.
    // Every `users.<name>` a module calls, read off its flattened text.
    let called = |text: &str| -> Vec<String> {
        text.split("users.")
            .skip(1)
            .map(|rest| rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect())
            .collect()
    };

    // api.py: what it imports from users, or calls on the module, has to be
    // there, and the handler has to call it.
    let api = flat("api.py");
    let imported = api.strip_prefix("fromusersimport").and_then(|rest| rest.split("def").next()).unwrap_or("");
    let handler_ok = if !imported.is_empty() {
        defines(users, imported) && api.contains(&format!("return{imported}(request"))
    } else {
        api.starts_with("importusers") && called(&api).iter().any(|n| n.contains("user") && defines(users, n))
    };
    if !handler_ok {
        failed.push("FAILED test_app.py::test_handler - ImportError: cannot import name from 'users'".to_string());
    }

    // jobs.py: the lookup for "user" and for "team" both have to resolve,
    // by a name built from a prefix or by naming the functions outright.
    let jobs = flat("jobs.py");
    let resolves = |kind: &str| -> bool {
        match jobs.split("getattr(users,\"").nth(1).and_then(|rest| rest.split('"').next()) {
            Some(prefix) if jobs.contains(&format!("getattr(users,\"{prefix}\"+kind)")) => defines(users, &format!("{prefix}{kind}")),
            _ => called(&jobs).iter().any(|n| n.ends_with(kind) && defines(users, n)) && called(&jobs).iter().all(|n| defines(users, n)),
        }
    };
    if !resolves("user") {
        failed.push("FAILED test_app.py::test_load - AttributeError: module 'users' has no attribute 'get_user'".to_string());
    } else if !resolves("team") {
        failed.push("FAILED test_app.py::test_load - AttributeError: module 'users' has no lookup for 'team'".to_string());
    }
    if failed.is_empty() {
        return "2 passed".into();
    }
    format!("{}\n{} failed, {} passed", failed.join("\n"), failed.len(), 2 - failed.len())
}

fn dispatch_grade(w: &Workspace, _: &str) -> Result<(), String> {
    if let Some((path, _)) = w.files().iter().find(|(path, text)| path != "test_app.py" && text.contains("get_user")) {
        return Err(format!("{path} still says get_user"));
    }
    if !defines(w.read("users.py").unwrap_or(""), "fetch_user") {
        return Err("users.py does not define fetch_user".into());
    }
    match dispatch_tests(w).as_str() {
        "2 passed" => Ok(()),
        other => Err(other.lines().next().unwrap_or("").to_string()),
    }
}

// Two functions alike to the letter but for their names, in a long file:
// the bug is in one, `edit_file` refuses the shared line as ambiguous, and
// the fix has to land in the right one.
const PARSE_US: &str = "def parse_date_us(text):\n    \"\"\"MM/DD/YYYY to (year, month, day).\"\"\"\n    month, day, year = text.split(\"/\")\n    return int(year), int(month), int(day)\n";
const PARSE_EU: &str = "def parse_date_eu(text):\n    \"\"\"DD/MM/YYYY to (year, month, day).\"\"\"\n    month, day, year = text.split(\"/\")\n    return int(year), int(month), int(day)\n";
const DATES_TEST: &str = "from formats import parse_date_eu, parse_date_us\n\n\n\
def test_us():\n    assert parse_date_us(\"12/31/2026\") == (2026, 12, 31)\n\n\n\
def test_eu():\n    assert parse_date_eu(\"31/12/2026\") == (2026, 12, 31)\n";

fn formats_text() -> String {
    let mut text = String::from("\"\"\"Number and date formats for the reports.\"\"\"\n\n\n");
    for i in 1..=120u32 {
        if i == 60 {
            text.push_str(PARSE_US);
            text.push_str("\n\n");
            text.push_str(PARSE_EU);
            text.push_str("\n\n");
        }
        text.push_str(&format!("def pad_{i:03}(value):\n    return str(value).rjust({})\n\n\n", i % 12 + 4));
    }
    text
}

fn formats_files() -> Workspace {
    Workspace::new(&[("formats.py", &formats_text()), ("test_formats.py", DATES_TEST)])
}

/// What one of the two functions returns for `input`, read off its body:
/// the names the split assigns, in order, and the names the return gives
/// back. `None` when the body is not that shape any more.
fn run_parse(body: &str, input: &str) -> Option<(u32, u32, u32)> {
    let flat: Vec<String> = body.lines().map(|l| l.split_whitespace().collect()).collect();
    let split = flat.iter().find(|l| l.ends_with("=text.split(\"/\")"))?;
    let names: Vec<&str> = split.trim_end_matches("=text.split(\"/\")").split(',').collect();
    let parts: Vec<&str> = input.split('/').collect();
    if names.len() != 3 || parts.len() != 3 {
        return None;
    }
    let value = |name: &str| -> Option<u32> { parts.get(names.iter().position(|n| *n == name)?)?.parse().ok() };
    let ret = flat.iter().find(|l| l.starts_with("return"))?.trim_start_matches("return").to_string();
    let out: Vec<u32> = ret.split(',').map(|v| value(v.trim_start_matches("int(").trim_end_matches(')'))).collect::<Option<_>>()?;
    (out.len() == 3).then(|| (out[0], out[1], out[2]))
}

fn formats_tests(w: &Workspace) -> String {
    if w.read("test_formats.py") != Some(DATES_TEST) {
        return "FAILED: test_formats.py was changed; the tests are not to be edited.".into();
    }
    let now = w.read("formats.py").unwrap_or("");
    let original = formats_text();
    // Everything but the EU function's body stays as it was.
    let (before_eu, after_eu) = original.split_once(PARSE_EU).expect("the EU parser is in the file");
    let eu_body = now.strip_prefix(before_eu).and_then(|rest| rest.strip_suffix(after_eu));
    let Some(eu_body) = eu_body.filter(|b| b.starts_with("def parse_date_eu(text):")) else {
        return "FAILED test_formats.py::test_us - formats.py changed outside parse_date_eu\n\
                FAILED test_formats.py::test_eu - formats.py changed outside parse_date_eu\n2 failed"
            .into();
    };
    match run_parse(eu_body, "31/12/2026") {
        Some((2026, 12, 31)) => "2 passed".into(),
        Some((y, m, d)) => format!("FAILED test_formats.py::test_eu - assert ({y}, {m}, {d}) == (2026, 12, 31)\n1 failed, 1 passed"),
        None => "FAILED test_formats.py::test_eu - ValueError: invalid literal for int()\n1 failed, 1 passed".into(),
    }
}

fn formats_grade(w: &Workspace, _: &str) -> Result<(), String> {
    match formats_tests(w).as_str() {
        "2 passed" => Ok(()),
        other => Err(other.lines().next().unwrap_or("").to_string()),
    }
}

/// The suite, in the order it runs.
pub const TASKS: [Task; 11] = [
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
    Task {
        name: "fix-a-bug-across-files",
        prompt: "The tests in this project fail. Find out why and fix the code, not the tests. Run the tests to check.",
        files: shop_files,
        tests: shop_tests,
        grade: shop_grade,
    },
    Task {
        name: "follow-the-includes",
        prompt: "What max_connections does this service run with? Answer with the number.",
        files: includes_files,
        tests: no_tests,
        grade: includes_grade,
    },
    Task {
        name: "complete-a-translation",
        prompt: "locales/es.json is missing some of the texts locales/en.json has. Add them, in Spanish.",
        files: locale_files,
        tests: no_tests,
        grade: locale_grade,
    },
    Task {
        name: "fix-in-a-long-file",
        prompt: "A test in this project fails. Fix the code so that it passes, and change nothing else.",
        files: tables_files,
        tests: tables_tests,
        grade: tables_grade,
    },
    Task {
        name: "total-from-a-table",
        prompt: "What did EU orders bring in during March 2026, not counting refunded ones? Answer with the amount to the cent.",
        files: orders_files,
        tests: no_tests,
        grade: orders_grade,
    },
    Task {
        name: "rename-past-a-dispatch",
        prompt: "Rename get_user to fetch_user everywhere in the project, its definition and every use, and keep the tests passing.",
        files: dispatch_files,
        tests: dispatch_tests,
        grade: dispatch_grade,
    },
    Task {
        name: "fix-one-of-two-twins",
        prompt: "A test in this project fails. Fix the code so that it passes, and change nothing else.",
        files: formats_files,
        tests: formats_tests,
        grade: formats_grade,
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
    /// The answer stopped at [`MAX_TOKENS`] rather than ending.
    pub cut: bool,
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
            // Anthropic's `input_tokens` leaves out the prompt read from the
            // cache, which the two cache fields carry: the sum is the prompt.
            let usage = answer.get("usage");
            turn.prompt_tokens = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                .iter()
                .map(|key| count(usage.and_then(|u| u.get(key))))
                .sum();
            turn.output_tokens = count(usage.and_then(|u| u.get("output_tokens")));
            turn.cut = answer.get("stop_reason").and_then(Value::as_str) == Some("max_tokens");
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
            turn.cut = answer
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("finish_reason"))
                .and_then(Value::as_str)
                == Some("length");
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
    let prompt: u64 = verdicts.iter().map(|v| v.prompt_tokens).sum();
    let output: u64 = verdicts.iter().map(|v| v.output_tokens).sum();
    let failed: Vec<&str> = verdicts.iter().filter(|v| !v.solved).map(|v| v.task).collect();
    format!(
        "RESULT agent_{}={solved}/{} agent_turns={turns} agent_calls={calls} agent_bad_calls={bad} \
         agent_seconds={seconds:.0} agent_prompt_tokens={prompt} agent_output_tokens={output} agent_failed={}",
        api.name(),
        verdicts.len(),
        if failed.is_empty() { "-".to_string() } else { failed.join(",") }
    )
}

/// What a missed task did, to read afterwards: why it failed, every call it
/// made with the first line of its answer, its last answer, and each file it
/// changed or added. A file under 200 lines is given whole; a longer one
/// only by the lines that differ, numbered, as one-line edits mostly are.
pub fn failure_report(verdict: &Verdict, start: &Workspace, end: &Workspace, calls: &[(Call, Outcome)], answer: &str) -> String {
    let mut out = format!("{} - {}\n\ncalls:\n", verdict.task, verdict.reason);
    for (call, outcome) in calls {
        let args: String = call.arguments.chars().take(300).collect();
        let more = if call.arguments.chars().count() > 300 { "..." } else { "" };
        let said = outcome.text.lines().next().unwrap_or("");
        out.push_str(&format!("  {}({args}{more})\n    -> {said}\n", call.name));
    }
    out.push_str(&format!("\nlast answer:\n{}\n", answer.trim()));
    for (path, text) in end.files() {
        let before = start.read(path);
        if before == Some(text.as_str()) {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        match before {
            Some(before) if lines.len() >= 200 => {
                out.push_str(&format!("\n--- {path} (changed; the lines that differ)\n"));
                let old: Vec<&str> = before.lines().collect();
                for i in 0..old.len().max(lines.len()) {
                    if old.get(i) != lines.get(i) {
                        out.push_str(&format!("{:>5} - {}\n{:>5} + {}\n", i + 1, old.get(i).unwrap_or(&""), i + 1, lines.get(i).unwrap_or(&"")));
                    }
                }
            }
            Some(_) => out.push_str(&format!("\n--- {path} (changed)\n{text}\n")),
            None => out.push_str(&format!("\n--- {path} (new)\n{text}\n")),
        }
    }
    out
}

/// One API's run of the suite, read back from its `RESULT` line.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    pub api: String,
    pub solved: u32,
    pub total: u32,
    pub turns: u32,
    pub calls: u32,
    pub bad_calls: u32,
    pub seconds: f64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    /// The tasks it did not solve, by name.
    pub failed: Vec<String>,
}

impl Summary {
    /// Every `agent_<api>=` line in a captured run, the last per API kept.
    pub fn read<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<Summary> {
        let mut out: Vec<Summary> = Vec::new();
        for line in lines {
            if !line.starts_with("RESULT") {
                continue;
            }
            let fields = crate::evalparse::fields(line);
            let Some((api, score)) = fields.iter().find_map(|(k, v)| {
                let api = k.strip_prefix("agent_")?;
                (api == "anthropic" || api == "openai").then(|| (api.to_string(), v.clone()))
            }) else {
                continue;
            };
            let Some((solved, total)) = score.split_once('/') else { continue };
            let get = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("");
            let summary = Summary {
                api: api.clone(),
                solved: solved.parse().unwrap_or(0),
                total: total.parse().unwrap_or(0),
                turns: get("agent_turns").parse().unwrap_or(0),
                calls: get("agent_calls").parse().unwrap_or(0),
                bad_calls: get("agent_bad_calls").parse().unwrap_or(0),
                seconds: get("agent_seconds").parse().unwrap_or(0.0),
                prompt_tokens: get("agent_prompt_tokens").parse().unwrap_or(0),
                output_tokens: get("agent_output_tokens").parse().unwrap_or(0),
                failed: match get("agent_failed") {
                    "" | "-" => Vec::new(),
                    list => list.split(',').map(str::to_string).collect(),
                },
            };
            out.retain(|s| s.api != api);
            out.push(summary);
        }
        out
    }
}

/// Whether a run clears `floor`, the share of tasks each API must solve: an
/// API that was not run does not pass, and no run at all is not a pass.
pub fn passes(summaries: &[Summary], floor: f64) -> bool {
    !summaries.is_empty()
        && summaries.iter().all(|s| s.total > 0 && s.solved as f64 >= floor * s.total as f64 - 1e-9)
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

    fn edit(w: &mut Workspace, path: &str, old: &str, new: &str) -> Outcome {
        let args = Value::object(vec![("path", Value::str(path)), ("old_text", Value::str(old)), ("new_text", Value::str(new))]);
        run(w, &call("edit_file", &args.to_json()), no_tests)
    }

    #[test]
    fn an_edit_needs_its_old_text_exactly_once() {
        let mut w = rename_files();
        assert_eq!(edit(&mut w, "api.py", "get_user(request", "fetch_user(request").text, "Edited api.py.");
        assert!(w.read("api.py").unwrap().contains("return fetch_user(request[\"id\"])"));
        let twice = edit(&mut w, "jobs.py", "get_user", "x");
        assert!(twice.valid && twice.text.starts_with("Error: old_text appears 2 times"), "{}", twice.text);
        assert!(edit(&mut w, "jobs.py", "nothing like it", "x").text.starts_with("Error: old_text does not appear"));
        assert!(edit(&mut w, "gone.py", "a", "b").text.starts_with("Error: there is no file"));
        assert_eq!(w.read("jobs.py"), Some(JOBS), "a refused edit changes nothing");
    }

    #[test]
    fn the_shop_is_fixed_in_the_invoice_not_in_the_rates() {
        let mut w = shop_files();
        assert_eq!(
            shop_tests(&w),
            "FAILED tests/test_shop.py::test_total - assert 1980.0 == 108.9\n1 failed, 1 passed"
        );
        // The rate made a fraction: the invoice passes, the export breaks.
        let mut rate = w.clone();
        rate.write("shop/rates.py", &RATES.replace("VAT = 21", "VAT = 0.21"));
        assert_eq!(
            shop_tests(&rate),
            "FAILED tests/test_shop.py::test_export - AssertionError: assert 'VAT;0.21' == 'VAT;21'\n1 failed, 1 passed"
        );
        assert!(shop_grade(&rate, "").is_err());
        edit(&mut w, "shop/invoice.py", "(1 + rates.VAT)", "(1 + rates.VAT / 100)");
        assert_eq!(shop_tests(&w), "2 passed");
        assert_eq!(shop_grade(&w, ""), Ok(()));
    }

    #[test]
    fn the_includes_override_and_the_decoy_does_not() {
        let w = includes_files();
        assert!(w.read("config/service.ini").unwrap().ends_with("include = profiles/base.ini, profiles/prod.ini\n"));
        assert!(includes_grade(&w, "It runs with 6000.").is_ok());
        for wrong in ["4711", "2048", "8000"] {
            assert!(includes_grade(&w, wrong).is_err());
        }
    }

    #[test]
    fn a_translation_keeps_what_was_there_and_its_placeholders() {
        let done = r#"{
  "app": {"title": "Notas", "save": "Guardar", "cancel": "Cancelar", "delete": "Eliminar"},
  "inbox": {"empty": "No hay mensajes", "unread": "Tienes {count} mensajes sin leer", "from": "De {name}"},
  "settings": {"language": "Idioma", "theme": "Tema", "dark": "Oscuro", "light": "Claro"}
}"#;
        let mut w = locale_files();
        assert_eq!(locale_grade(&w, ""), Err("es.json has no app.delete".into()));
        w.write("locales/es.json", done);
        assert_eq!(locale_grade(&w, ""), Ok(()));
        w.write("locales/es.json", &done.replace("De {name}", "De nombre"));
        assert_eq!(locale_grade(&w, ""), Err("es.json's inbox.from lost {name}".into()));
        w.write("locales/es.json", &done.replace("\"Claro\"", "\"Luz\""));
        assert!(locale_grade(&w, "").unwrap_err().starts_with("es.json changed settings.light"));
        w.write("locales/es.json", &done.replace("\"Oscuro\"", "\"Dark\""));
        assert_eq!(locale_grade(&w, ""), Err("es.json's settings.dark is still English".into()));
        w.write("locales/es.json", &done[..done.len() - 2]);
        assert!(locale_grade(&w, "").unwrap_err().starts_with("es.json is not JSON"));
    }

    #[test]
    fn the_long_file_takes_one_line_and_nothing_else() {
        let mut w = tables_files();
        let text = w.read("tables.py").unwrap().to_string();
        assert!(text.lines().count() > 1200, "{}", text.lines().count());
        assert!(text.contains("def scale_0150(x):\n    return x * 1.0150\n"));
        assert_eq!(tables_tests(&w), "FAILED test_tables.py::test_checksum - assert 0 == 128\n1 failed, 1 passed");
        // A rewrite that dropped the tail is a failure, whatever it fixed.
        let mut cut = w.clone();
        cut.write("tables.py", &text[..text.len() / 2].replace("% 255", "% 256"));
        assert!(tables_tests(&cut).starts_with("FAILED test_tables.py::test_scale"));
        assert_eq!(edit(&mut w, "tables.py", "sum(data) % 255", "sum(data) % 256").text, "Edited tables.py.");
        assert_eq!(tables_grade(&w, ""), Ok(()));
    }

    #[test]
    fn the_table_total_is_the_eu_march_paid_rows_to_the_cent() {
        let w = orders_files();
        let csv = w.read("data/orders.csv").unwrap();
        assert_eq!(csv.lines().count(), 61);
        let total = orders_total();
        // Enough rows that a sum by eye is work, and a refund among them to
        // leave out: the question's rule has a row to apply to.
        let rows: Vec<&str> = csv.lines().filter(|l| l.contains(",2026-03-") && l.contains(",EU,")).collect();
        assert!(rows.len() >= 4, "{rows:?}");
        assert!(rows.iter().any(|l| l.ends_with(",refunded")), "{rows:?}");
        assert!(orders_grade(&w, &format!("EU brought in {total} EUR.")).is_ok());
        let (euros, cents) = total.split_once('.').unwrap();
        let grouped = format!("{},{:03}.{cents}", euros.parse::<u64>().unwrap() / 1000, euros.parse::<u64>().unwrap() % 1000);
        assert!(orders_grade(&w, &grouped).is_ok(), "{grouped}");
        // The month's EU rows with the refunds counted are not the answer.
        let all: u64 = orders().iter().filter(|o| o.2 == "EU" && o.1.starts_with("2026-03")).map(|o| o.3).sum();
        assert!(orders_grade(&w, &format!("{}.{:02}", all / 100, all % 100)).is_err());
    }

    #[test]
    fn a_rename_has_to_see_past_the_built_name() {
        let w = dispatch_files();
        assert_eq!(dispatch_tests(&w), "2 passed");
        assert_eq!(dispatch_grade(&w, ""), Err("users.py still says get_user".into()));
        // The text-search rename: every get_user gone, the dispatch broken.
        let mut naive = w.clone();
        for path in ["users.py", "api.py"] {
            let text = naive.read(path).unwrap().replace("get_user", "fetch_user");
            naive.write(path, &text);
        }
        assert!(dispatch_tests(&naive).contains("test_load - AttributeError"), "{}", dispatch_tests(&naive));
        assert!(dispatch_grade(&naive, "").is_err());
        // "fetch_" + kind fixes users and breaks teams, unless get_team goes too.
        let mut prefix = naive.clone();
        prefix.write("jobs.py", &DISPATCH_JOBS.replace("\"get_\" + kind", "\"fetch_\" + kind"));
        assert!(dispatch_tests(&prefix).contains("no lookup for 'team'"));
        let mut both = prefix.clone();
        both.write("users.py", &naive.read("users.py").unwrap().replace("get_team", "fetch_team"));
        assert_eq!(dispatch_grade(&both, ""), Ok(()));
        // Or name the functions outright.
        let mut table = naive.clone();
        table.write(
            "jobs.py",
            "import users\n\n\ndef load(kind, key):\n    return {\"user\": users.fetch_user, \"team\": users.get_team}[kind](key)\n",
        );
        assert_eq!(dispatch_grade(&table, ""), Ok(()));
    }

    #[test]
    fn only_the_eu_twin_may_change_and_it_has_to_parse_right() {
        let mut w = formats_files();
        let text = w.read("formats.py").unwrap().to_string();
        assert_eq!(text.matches("    month, day, year = text.split(\"/\")\n").count(), 2);
        assert_eq!(formats_tests(&w), "FAILED test_formats.py::test_eu - assert (2026, 31, 12) == (2026, 12, 31)\n1 failed, 1 passed");
        // The shared line is ambiguous to edit_file.
        assert!(edit(&mut w, "formats.py", "    month, day, year = text.split(\"/\")", "x").text.contains("appears 2 times"));
        // Fixing the US twin instead breaks it and leaves the EU one.
        let mut wrong = w.clone();
        wrong.write("formats.py", &text.replacen("    month, day, year", "    day, month, year", 1));
        assert!(formats_tests(&wrong).contains("changed outside parse_date_eu"));
        // Either line of the EU twin can carry the fix.
        let mut split = w.clone();
        split.write("formats.py", &text.replace(PARSE_EU, &PARSE_EU.replace("month, day, year =", "day, month, year =")));
        assert_eq!(formats_grade(&split, ""), Ok(()));
        let mut ret = w.clone();
        ret.write("formats.py", &text.replace(PARSE_EU, &PARSE_EU.replace("int(month), int(day)", "int(day), int(month)")));
        assert_eq!(formats_grade(&ret, ""), Ok(()));
    }

    #[test]
    fn requests_and_answers_round_trip_on_both_apis() {
        let task = &TASKS[0];
        let anthropic = first_request(Api::Anthropic, "m", task);
        assert!(anthropic.to_json().contains(r#""system":"You are a coding agent"#));
        assert_eq!(anthropic.get("tools").and_then(Value::as_array).map(Vec::len), Some(6));
        let openai = first_request(Api::OpenAi, "m", task);
        assert_eq!(openai.get("messages").and_then(Value::as_array).map(Vec::len), Some(2));

        let answer = json::parse(
            r#"{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"read_file","input":{"path":"calc.py"}}],"usage":{"input_tokens":100,"cache_read_input_tokens":800,"output_tokens":20}}"#,
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
        assert!(!turn.cut);
        let cut = json::parse(r#"{"choices":[{"finish_reason":"length","message":{"content":""}}]}"#).unwrap();
        assert!(read_turn(Api::OpenAi, &cut).unwrap().cut);
        let cut = json::parse(r#"{"content":[{"type":"thinking","thinking":"..."}],"stop_reason":"max_tokens"}"#).unwrap();
        assert!(read_turn(Api::Anthropic, &cut).unwrap().cut);
        assert_eq!((turn.calls[0].name.as_str(), turn.output_tokens), ("run_tests", 2));
        let mut body = openai.clone();
        continue_request(Api::OpenAi, &mut body, &answer, &[(turn.calls[0].clone(), outcome)]);
        assert_eq!(body.get("messages").and_then(Value::as_array).map(Vec::len), Some(4));
    }

    #[test]
    fn the_result_line_sums_the_run() {
        let v = |task, solved, calls, bad| Verdict {
            task,
            solved,
            calls,
            bad_calls: bad,
            turns: 3,
            seconds: 10.4,
            prompt_tokens: 1000,
            output_tokens: 50,
            ..Verdict::default()
        };
        let line = result_line(Api::OpenAi, &[v("a", true, 4, 0), v("b", false, 6, 1)]);
        assert_eq!(
            line,
            "RESULT agent_openai=1/2 agent_turns=6 agent_calls=10 agent_bad_calls=1 agent_seconds=21 \
             agent_prompt_tokens=2000 agent_output_tokens=100 agent_failed=b"
        );
        let all = result_line(Api::Anthropic, &[v("a", true, 4, 0)]);
        assert!(all.ends_with("agent_failed=-"), "{all}");

        let read = Summary::read([all.as_str(), "noise", line.as_str()]);
        assert_eq!(read.len(), 2);
        assert_eq!((read[0].api.as_str(), read[0].solved, read[0].total), ("anthropic", 1, 1));
        assert!(read[0].failed.is_empty());
        assert_eq!(read[1].failed, vec!["b".to_string()]);
        assert_eq!((read[1].calls, read[1].bad_calls, read[1].prompt_tokens), (10, 1, 2000));
    }

    #[test]
    fn a_failure_report_shows_the_calls_and_only_what_changed() {
        let start = dispatch_files();
        let mut end = start.clone();
        end.write("users.py", &DISPATCH_USERS.replace("def get_user", "def fetch_user"));
        end.write("notes.md", "renamed\n");
        let calls = vec![(call("search", r#"{"text":"get_user"}"#), Outcome { text: "users.py:1: def get_user(user_id):\nmore".into(), valid: true })];
        let verdict = Verdict { task: "rename-past-a-dispatch", reason: "api.py still says get_user".into(), ..Verdict::default() };
        let report = failure_report(&verdict, &start, &end, &calls, " Done. ");
        assert!(report.starts_with("rename-past-a-dispatch - api.py still says get_user\n"), "{report}");
        assert!(report.contains("  search({\"text\":\"get_user\"})\n    -> users.py:1: def get_user(user_id):\n"), "{report}");
        assert!(report.contains("last answer:\nDone.\n"));
        assert!(report.contains("--- users.py (changed)\ndef fetch_user"));
        assert!(report.contains("--- notes.md (new)\nrenamed"));
        assert!(!report.contains("--- jobs.py"), "unchanged files are left out");

        // A long file by its differing lines alone.
        let start = tables_files();
        let mut end = start.clone();
        let text = end.read("tables.py").unwrap().replace("% 255", "% 254");
        end.write("tables.py", &text);
        let report = failure_report(&verdict, &start, &end, &[], "");
        assert!(report.contains("(changed; the lines that differ)\n"), "{report}");
        assert!(report.contains(" -     return sum(data) % 255\n") && report.contains(" +     return sum(data) % 254\n"), "{report}");
        assert!(report.lines().count() < 20, "{report}");
    }

    #[test]
    fn the_floor_holds_on_every_api_that_ran() {
        let s = |api: &str, solved, total| Summary { api: api.into(), solved, total, ..Summary::default() };
        assert!(passes(&[s("anthropic", 6, 8), s("openai", 7, 8)], 0.75));
        assert!(!passes(&[s("anthropic", 5, 8), s("openai", 8, 8)], 0.75));
        assert!(!passes(&[], 0.0), "no run is not a pass");
        assert!(!passes(&[s("openai", 0, 0)], 0.0));
    }
}
