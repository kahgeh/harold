use std::os::unix::fs::PermissionsExt;

use super::*;

fn job(folder: &std::path::Path) -> ModelJob {
    ModelJob {
        folder: folder.to_path_buf(),
        model: "haiku".into(),
        system_prompt: "RULES\n\nSKILL BODY".into(),
        allowed_tools: None,
        add_dirs: Vec::new(),
        run_budget_usd_micros: None,
        facts_json: br#"{"now":"x"}"#.to_vec(),
        timeout: Duration::from_secs(10),
    }
}

fn texts(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.to_str().unwrap().to_string())
        .collect()
}

fn position(args: &[String], flag: &str) -> Option<usize> {
    args.iter().position(|arg| arg == flag)
}

#[test]
fn the_fixed_flags_and_the_model_are_always_passed() {
    let args = texts(claude_args(&job(std::path::Path::new("/tmp")), false));
    assert_eq!(
        &args[..3],
        ["--print", "--safe-mode", "--no-session-persistence"]
    );
    let model = position(&args, "--model").unwrap();
    assert_eq!(args[model + 1], "haiku");
    let format = position(&args, "--output-format").unwrap();
    assert_eq!(args[format + 1], "json");
    let prompt = position(&args, "--system-prompt").unwrap();
    assert_eq!(args[prompt + 1], "RULES\n\nSKILL BODY");
}

#[test]
fn a_run_budget_is_passed_only_when_the_run_has_one() {
    let mut asked = job(std::path::Path::new("/tmp"));
    assert!(position(&texts(claude_args(&asked, false)), "--max-budget-usd").is_none());
    asked.run_budget_usd_micros = Some(30_000);
    let args = texts(claude_args(&asked, false));
    assert_eq!(
        args[position(&args, "--max-budget-usd").unwrap() + 1],
        "0.03"
    );
    asked.run_budget_usd_micros = Some(10_000);
    let args = texts(claude_args(&asked, false));
    assert_eq!(
        args[position(&args, "--max-budget-usd").unwrap() + 1],
        "0.01"
    );
    asked.run_budget_usd_micros = Some(1_500_000);
    let args = texts(claude_args(&asked, false));
    assert_eq!(
        args[position(&args, "--max-budget-usd").unwrap() + 1],
        "1.5"
    );
}

#[test]
fn max_turns_is_passed_only_when_accepted() {
    let asked = job(std::path::Path::new("/tmp"));
    assert!(position(&texts(claude_args(&asked, false)), "--max-turns").is_none());
    let args = texts(claude_args(&asked, true));
    assert_eq!(args[position(&args, "--max-turns").unwrap() + 1], "8");
}

#[test]
fn allowed_tools_reach_the_flag_exactly_as_written_and_are_left_out_when_none() {
    let mut asked = job(std::path::Path::new("/tmp"));
    assert!(position(&texts(claude_args(&asked, false)), "--allowedTools").is_none());
    for tools in [
        "Bash(git push:*)",
        "Write",
        "Read(~/**)",
        "Bash(bin/pod slots:*), Bash(git worktree list:*)",
        "  odd  spacing,Write ",
    ] {
        asked.allowed_tools = Some(tools.into());
        let args = texts(claude_args(&asked, false));
        assert_eq!(args[position(&args, "--allowedTools").unwrap() + 1], tools);
    }
}

#[test]
fn add_dirs_become_one_flag_each_in_order_before_allowed_tools() {
    let mut asked = job(std::path::Path::new("/tmp"));
    asked.add_dirs = vec!["/h/bin/harold".into(), "/with space/x".into()];
    asked.allowed_tools = Some("Write".into());
    let args = texts(claude_args(&asked, false));
    let flags: Vec<usize> = (0..args.len())
        .filter(|i| args[*i] == "--add-dir")
        .collect();
    assert_eq!(flags.len(), 2);
    assert_eq!(args[flags[0] + 1], "/h/bin/harold");
    assert_eq!(args[flags[1] + 1], "/with space/x");
    assert!(flags[1] < position(&args, "--allowedTools").unwrap());
}

#[test]
fn no_add_dir_flag_when_the_list_is_empty() {
    let args = texts(claude_args(&job(std::path::Path::new("/tmp")), false));
    assert!(position(&args, "--add-dir").is_none());
}

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-runner-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn script(&self, name: &str, body: &str) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path_env() -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap()),
        ("HOME".into(), "/home/watch".into()),
    ]
}

#[tokio::test]
async fn the_process_runs_in_the_folder_with_a_cleared_environment_and_the_facts_on_stdin() {
    let temp = Temp::new();
    let work = Temp::new();
    // A fake program stands in for Claude Code: no test runs `claude`.
    let cli = temp.script(
        "fake-claude",
        r#"pwd > out.txt; env >> out.txt; echo "--ARGS" >> out.txt; for a in "$@"; do echo "$a" >> out.txt; done; cat > facts.txt
echo '{"subtype":"success","is_error":false,"result":"QUIET","total_cost_usd":0.01}'"#,
    );
    let runner = ClaudeRunner::new(cli, path_env()).with_max_turns(true);
    let mut asked = job(&work.0);
    asked.allowed_tools = Some("Bash(git status:*)".into());

    let result = runner.run(&asked).await;

    let ModelResult::Exited { stdout, ok, .. } = result else {
        panic!("{result:?}")
    };
    assert!(ok);
    assert!(String::from_utf8(stdout).unwrap().contains("\"QUIET\""));
    let out = std::fs::read_to_string(work.0.join("out.txt")).unwrap();
    assert!(out.starts_with(work.0.to_str().unwrap()), "cwd: {out}");
    assert!(out.contains("HOME=/home/watch"));
    assert!(
        !out.lines().any(|line| line.starts_with("CARGO")),
        "the environment was not cleared: {out}"
    );
    let (env_part, args_part) = out.split_once("--ARGS").unwrap();
    assert!(env_part.contains("PATH="));
    assert!(args_part.contains("--max-turns\n8"));
    assert!(args_part.contains("--allowedTools\nBash(git status:*)"));
    assert_eq!(
        std::fs::read_to_string(work.0.join("facts.txt")).unwrap(),
        r#"{"now":"x"}"#
    );
}

#[tokio::test]
async fn a_program_that_cannot_start_is_failed_to_start() {
    let work = Temp::new();
    let runner = ClaudeRunner::new("/nonexistent/claude".into(), path_env());
    assert_eq!(runner.run(&job(&work.0)).await, ModelResult::FailedToStart);
}

#[tokio::test]
async fn a_failing_exit_is_reported_with_its_output() {
    let temp = Temp::new();
    let work = Temp::new();
    let cli = temp.script("fake-claude", "cat > /dev/null; echo oops; exit 3");
    let runner = ClaudeRunner::new(cli, path_env());
    assert_eq!(
        runner.run(&job(&work.0)).await,
        ModelResult::Exited {
            stdout: b"oops\n".to_vec(),
            ok: false,
            exit: Exit {
                code: Some(3),
                signal: None
            }
        }
    );
}

#[tokio::test]
async fn a_child_killed_by_a_signal_reports_the_signal() {
    let temp = Temp::new();
    let work = Temp::new();
    let cli = temp.script("fake-claude", "cat > /dev/null; kill -TERM $$");
    let runner = ClaudeRunner::new(cli, path_env());
    let ModelResult::Exited { ok, exit, .. } = runner.run(&job(&work.0)).await else {
        panic!("expected an exit");
    };
    assert!(!ok);
    assert_eq!(
        exit,
        Exit {
            code: None,
            signal: Some(15)
        }
    );
}

fn process_exists(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn a_run_over_time_is_killed_with_the_child_it_started() {
    let temp = Temp::new();
    let work = Temp::new();
    let cli = temp.script(
        "fake-claude",
        "sleep 300 &\necho $! > child.pid\ncat > /dev/null\nsleep 300",
    );
    let runner = ClaudeRunner::new(cli, path_env());
    let mut asked = job(&work.0);
    asked.timeout = Duration::from_secs(3);

    assert_eq!(runner.run(&asked).await, ModelResult::Killed);

    let pid = std::fs::read_to_string(work.0.join("child.pid"))
        .expect("the fake CLI did not write child.pid before the 3 s run limit");
    let pid = pid.trim();
    let mut gone = false;
    for _ in 0..200 {
        if !process_exists(pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the child {pid} outlived the run");
}

#[test]
fn usd_text_has_no_trailing_zeros() {
    assert_eq!(usd_text(10_000), "0.01");
    assert_eq!(usd_text(50_000), "0.05");
    assert_eq!(usd_text(1_000_000), "1");
    assert_eq!(usd_text(123_456), "0.123456");
}
