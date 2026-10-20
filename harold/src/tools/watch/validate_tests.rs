use std::os::unix::fs::symlink;

use super::*;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-watch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    /// A folder with `.claude/skills/<skill>/SKILL.md` holding `text`.
    fn with_skill(skill: &str, text: &str) -> Self {
        let temp = Self::new();
        let dir = temp.0.join(".claude/skills").join(skill);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), text).unwrap();
        temp
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn request(folder: &Path) -> StartWatchRequest {
    StartWatchRequest {
        name: "after-release".into(),
        folder: folder.to_str().unwrap().into(),
        skill: "pod-watch".into(),
        every_minutes: 10,
        for_minutes: 120,
        report_to: String::new(),
        run_budget_usd: 0.0,
        every_run: false,
    }
}

fn check(request: &StartWatchRequest) -> Result<Checked, Refusal> {
    check_start(request, &WatchSettings::default())
}

#[test]
fn a_valid_request_is_stored_with_the_canonical_folder() {
    let temp = Temp::with_skill("pod-watch", "Check.");
    let mut asked = request(&temp.0);
    asked.folder = format!("{}/", temp.0.display());
    let checked = check(&asked).unwrap();
    assert_eq!(checked.folder, temp.0);
    assert_eq!(checked.report_to, ReportTo::SelfPane);
    assert_eq!(checked.run_budget_usd_micros, None);

    let link = Temp::new();
    symlink(&temp.0, link.0.join("through")).unwrap();
    asked.folder = link.0.join("through").to_str().unwrap().into();
    assert_eq!(check(&asked).unwrap().folder, temp.0);
}

#[test]
fn names_outside_the_rule_are_refused() {
    let temp = Temp::with_skill("pod-watch", "x");
    for name in ["", &"a".repeat(65), "a b", "a/b", "é"] {
        let mut asked = request(&temp.0);
        asked.name = name.into();
        assert_eq!(check(&asked), Err(Refusal::Invalid("name")), "{name:?}");
    }
    let mut asked = request(&temp.0);
    asked.name = "a".repeat(64);
    assert!(check(&asked).is_ok());
}

#[test]
fn every_for_and_report_to_outside_the_table_are_refused() {
    let temp = Temp::with_skill("pod-watch", "x");
    for (every, ok) in [(1, false), (2, true), (240, true), (241, false)] {
        let mut asked = request(&temp.0);
        asked.every_minutes = every;
        assert_eq!(check(&asked).is_ok(), ok, "every {every}");
    }
    for (minutes, ok) in [(0, false), (1, true), (1440, true), (1441, false)] {
        let mut asked = request(&temp.0);
        asked.for_minutes = minutes;
        assert_eq!(check(&asked).is_ok(), ok, "for {minutes}");
    }
    for (report_to, expected) in [
        ("chief", Ok(ReportTo::Chief)),
        ("self", Ok(ReportTo::SelfPane)),
        ("", Ok(ReportTo::SelfPane)),
        ("/Users/x/Dev/p/other", Err(Refusal::Invalid("report_to"))),
        ("Chief", Err(Refusal::Invalid("report_to"))),
    ] {
        let mut asked = request(&temp.0);
        asked.report_to = report_to.into();
        assert_eq!(check(&asked).map(|checked| checked.report_to), expected);
    }
}

#[test]
fn a_folder_that_is_relative_missing_or_a_file_is_invalid() {
    let temp = Temp::with_skill("pod-watch", "x");
    std::fs::write(temp.0.join("file"), "x").unwrap();
    for folder in [
        "relative/path".to_string(),
        String::new(),
        temp.0.join("missing").to_str().unwrap().to_string(),
        temp.0.join("file").to_str().unwrap().to_string(),
    ] {
        let mut asked = request(&temp.0);
        asked.folder = folder.clone();
        assert_eq!(check(&asked), Err(Refusal::Invalid("folder")), "{folder}");
    }
}

#[test]
fn a_skill_name_with_a_slash_or_a_dot_is_invalid_and_a_missing_one_is_unknown() {
    let temp = Temp::with_skill("pod-watch", "x");
    for skill in ["../x", "a/b", ".", "..", "", "a.b"] {
        let mut asked = request(&temp.0);
        asked.skill = skill.into();
        assert_eq!(check(&asked), Err(Refusal::Invalid("skill")), "{skill:?}");
    }
    let mut asked = request(&temp.0);
    asked.skill = "nope".into();
    assert_eq!(check(&asked), Err(Refusal::UnknownSkill));
}

#[test]
fn a_skill_that_leaves_the_skills_folder_is_unknown() {
    let outside = Temp::with_skill("real", "outside");
    let temp = Temp::new();
    let skills = temp.0.join(".claude/skills");
    std::fs::create_dir_all(&skills).unwrap();
    // The skill's folder is a link to a folder outside `.claude/skills/`.
    symlink(
        outside.0.join(".claude/skills/real"),
        skills.join("pod-watch"),
    )
    .unwrap();
    assert_eq!(check(&request(&temp.0)), Err(Refusal::UnknownSkill));
    assert_eq!(
        read_skill(&temp.0, "pod-watch"),
        Err(SkillMissing),
        "the run looks again"
    );

    // `SKILL.md` itself is a link to a file outside.
    let temp = Temp::new();
    let dir = temp.0.join(".claude/skills/pod-watch");
    std::fs::create_dir_all(&dir).unwrap();
    symlink(
        outside.0.join(".claude/skills/real/SKILL.md"),
        dir.join("SKILL.md"),
    )
    .unwrap();
    assert_eq!(check(&request(&temp.0)), Err(Refusal::UnknownSkill));

    // A link that stays inside `.claude/skills/` is fine.
    let temp = Temp::with_skill("shared", "x");
    symlink(
        temp.0.join(".claude/skills/shared"),
        temp.0.join(".claude/skills/pod-watch"),
    )
    .unwrap();
    assert!(check(&request(&temp.0)).is_ok());
}

#[test]
fn run_budgets_follow_the_settings() {
    let temp = Temp::with_skill("pod-watch", "x");
    let with = |budget: f64, cfg: WatchSettings| {
        let mut asked = request(&temp.0);
        asked.run_budget_usd = budget;
        check_start(&asked, &cfg).map(|checked| checked.run_budget_usd_micros)
    };
    let none = WatchSettings::default();
    assert_eq!(with(0.0, none.clone()), Ok(None));
    assert_eq!(with(0.03, none.clone()), Ok(Some(30_000)));
    assert_eq!(with(0.01, none.clone()), Ok(Some(10_000)));
    assert_eq!(
        with(0.005, none.clone()),
        Err(Refusal::Invalid("run_budget_usd"))
    );
    assert_eq!(
        with(-1.0, none.clone()),
        Err(Refusal::Invalid("run_budget_usd"))
    );
    assert_eq!(
        with(f64::NAN, none.clone()),
        Err(Refusal::Invalid("run_budget_usd"))
    );
    // No `max_run_budget_usd`: 0.50 is accepted.
    assert_eq!(with(0.5, none.clone()), Ok(Some(500_000)));
    let capped = WatchSettings {
        max_run_budget_usd: Some(0.10),
        ..none.clone()
    };
    assert_eq!(
        with(0.5, capped.clone()),
        Err(Refusal::Invalid("run_budget_usd"))
    );
    assert_eq!(with(0.10, capped), Ok(Some(100_000)));
    // `start` gave none: the setting applies when set.
    let default_budget = WatchSettings {
        run_budget_usd: Some(0.05),
        ..none
    };
    assert_eq!(with(0.0, default_budget), Ok(Some(50_000)));
}

#[test]
fn allowed_tools_come_back_exactly_as_written() {
    for tools in [
        "Bash(git push:*)",
        "Write",
        "Read(~/**)",
        "Bash(bin/pod slots:*), Bash(git worktree list:*), Bash(tmux capture-pane -p:*)",
    ] {
        let text =
            format!("---\nname: pod-watch\nallowed-tools: {tools}\n---\nCheck every product.\n");
        let temp = Temp::with_skill("pod-watch", &text);
        let skill = read_skill(&temp.0, "pod-watch").unwrap();
        assert_eq!(skill.allowed_tools.as_deref(), Some(tools));
        assert_eq!(skill.body, "Check every product.\n");
    }
}

#[test]
fn a_skill_without_allowed_tools_or_frontmatter_has_none() {
    let temp = Temp::with_skill("pod-watch", "---\nname: pod-watch\n---\nBody\n");
    let skill = read_skill(&temp.0, "pod-watch").unwrap();
    assert_eq!((skill.allowed_tools, skill.body.as_str()), (None, "Body\n"));
    let temp = Temp::with_skill("pod-watch", "Just text\n");
    let skill = read_skill(&temp.0, "pod-watch").unwrap();
    assert_eq!(
        (skill.allowed_tools, skill.body.as_str()),
        (None, "Just text\n")
    );
    // Frontmatter that never closes is not frontmatter, and its tools are not read.
    let temp = Temp::with_skill("pod-watch", "---\nallowed-tools: Write\nno end\n");
    assert_eq!(
        read_skill(&temp.0, "pod-watch").unwrap().allowed_tools,
        None
    );
}

#[test]
fn a_deleted_skill_is_missing() {
    let temp = Temp::with_skill("pod-watch", "x");
    std::fs::remove_file(temp.0.join(".claude/skills/pod-watch/SKILL.md")).unwrap();
    assert_eq!(read_skill(&temp.0, "pod-watch"), Err(SkillMissing));
    assert_eq!(
        read_skill(&temp.0.join("gone"), "pod-watch"),
        Err(SkillMissing)
    );
}
