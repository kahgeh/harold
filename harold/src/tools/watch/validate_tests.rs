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
        read_skill(&temp.0, "pod-watch", &[]),
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
        let skill = read_skill(&temp.0, "pod-watch", &[]).unwrap();
        assert_eq!(skill.allowed_tools.as_deref(), Some(tools));
        assert_eq!(skill.body, "Check every product.\n");
    }
}

#[test]
fn a_yaml_list_of_allowed_tools_sets_the_flag_and_no_value() {
    let read = |frontmatter: &str| {
        let text = format!("---\nname: pod-watch\n{frontmatter}---\nBody\n");
        let temp = Temp::with_skill("pod-watch", &text);
        let skill = read_skill(&temp.0, "pod-watch", &[]).unwrap();
        (skill.allowed_tools, skill.tools_unread)
    };
    assert_eq!(
        read("allowed-tools: Write\n"),
        (Some("Write".into()), false)
    );
    assert_eq!(
        read("allowed-tools:\n  - Bash(x)\n  - Read\n"),
        (None, true)
    );
    assert_eq!(read("allowed-tools:\n- Read\n"), (None, true));
    assert_eq!(read("allowed-tools:\nadd-dirs: ~/x\n"), (None, false));
    assert_eq!(
        read("allowed-tools: Write\n  - Read\n"),
        (Some("Write".into()), false)
    );
}

#[test]
fn a_skill_without_allowed_tools_or_frontmatter_has_none() {
    let temp = Temp::with_skill("pod-watch", "---\nname: pod-watch\n---\nBody\n");
    let skill = read_skill(&temp.0, "pod-watch", &[]).unwrap();
    assert_eq!((skill.allowed_tools, skill.body.as_str()), (None, "Body\n"));
    let temp = Temp::with_skill("pod-watch", "Just text\n");
    let skill = read_skill(&temp.0, "pod-watch", &[]).unwrap();
    assert_eq!(
        (skill.allowed_tools, skill.body.as_str()),
        (None, "Just text\n")
    );
    // Frontmatter that never closes is not frontmatter, and its tools are not read.
    let temp = Temp::with_skill("pod-watch", "---\nallowed-tools: Write\nno end\n");
    assert_eq!(
        read_skill(&temp.0, "pod-watch", &[]).unwrap().allowed_tools,
        None
    );
}

#[test]
fn a_deleted_skill_is_missing() {
    let temp = Temp::with_skill("pod-watch", "x");
    std::fs::remove_file(temp.0.join(".claude/skills/pod-watch/SKILL.md")).unwrap();
    assert_eq!(read_skill(&temp.0, "pod-watch", &[]), Err(SkillMissing));
    assert_eq!(
        read_skill(&temp.0.join("gone"), "pod-watch", &[]),
        Err(SkillMissing)
    );
}

/// A folder with the skill kept in a watch skill folder, linked into `.claude/skills/`.
struct Linked {
    work: Temp,
    kept: Temp,
}

impl Linked {
    /// `kept/<dir>/pod-watch/SKILL.md`, linked from `work/.claude/skills/pod-watch`.
    fn new(dir: &str) -> Self {
        let kept = Temp::new();
        let skill = kept.0.join(dir).join("pod-watch");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nallowed-tools: Write\n---\nLinked.\n",
        )
        .unwrap();
        let work = Temp::new();
        let skills = work.0.join(".claude/skills");
        std::fs::create_dir_all(&skills).unwrap();
        symlink(&skill, skills.join("pod-watch")).unwrap();
        Self { work, kept }
    }

    fn listed(&self, dir: &str) -> WatchSettings {
        WatchSettings {
            skill_folders: vec![self.kept.0.join(dir).to_str().unwrap().to_string()],
            ..WatchSettings::default()
        }
    }
}

#[test]
fn a_skill_linked_into_a_listed_folder_is_accepted_and_read_from_there() {
    let linked = Linked::new("watches");
    let cfg = linked.listed("watches");
    assert!(check_start(&request(&linked.work.0), &cfg).is_ok());
    let skill = read_skill(&linked.work.0, "pod-watch", &cfg.skill_folders).unwrap();
    assert_eq!(skill.allowed_tools.as_deref(), Some("Write"));
    assert_eq!(skill.body, "Linked.\n");
}

#[test]
fn a_linked_skill_is_refused_when_skill_folders_is_unset_or_lists_another_folder() {
    let linked = Linked::new("watches");
    assert_eq!(
        check_start(&request(&linked.work.0), &WatchSettings::default()),
        Err(Refusal::UnknownSkill)
    );
    let other = WatchSettings {
        skill_folders: vec![
            "/no/such/folder".into(),
            linked.kept.0.join("skills").to_str().unwrap().into(),
        ],
        ..WatchSettings::default()
    };
    assert_eq!(
        check_start(&request(&linked.work.0), &other),
        Err(Refusal::UnknownSkill)
    );
    assert_eq!(
        read_skill(&linked.work.0, "pod-watch", &other.skill_folders),
        Err(SkillMissing)
    );
}

#[test]
fn a_sibling_folder_whose_name_only_starts_the_same_is_not_inside_a_listed_folder() {
    // The skill is kept in `watches-old`, and `watches` is listed.
    let linked = Linked::new("watches-old");
    std::fs::create_dir_all(linked.kept.0.join("watches")).unwrap();
    let cfg = linked.listed("watches");
    assert_eq!(
        check_start(&request(&linked.work.0), &cfg),
        Err(Refusal::UnknownSkill)
    );
    // And a skill in another folder of the same repository.
    let linked = Linked::new("skills");
    std::fs::create_dir_all(linked.kept.0.join("watches")).unwrap();
    let cfg = linked.listed("watches");
    assert_eq!(
        check_start(&request(&linked.work.0), &cfg),
        Err(Refusal::UnknownSkill)
    );
}

#[test]
fn a_listed_folder_that_is_itself_a_link_is_compared_by_its_real_path() {
    let linked = Linked::new("real-watches");
    symlink(
        linked.kept.0.join("real-watches"),
        linked.kept.0.join("via-link"),
    )
    .unwrap();
    let cfg = linked.listed("via-link");
    assert!(check_start(&request(&linked.work.0), &cfg).is_ok());
}

#[test]
fn a_listed_folder_that_is_missing_matches_nothing_and_an_own_skill_still_works() {
    let temp = Temp::with_skill("pod-watch", "x");
    let cfg = WatchSettings {
        skill_folders: vec!["/Users/nobody/not-cloned/skills/watches".into()],
        ..WatchSettings::default()
    };
    assert!(check_start(&request(&temp.0), &cfg).is_ok());
}

#[test]
fn a_link_moved_outside_every_listed_folder_makes_the_run_find_no_skill() {
    let linked = Linked::new("watches");
    let cfg = linked.listed("watches");
    assert!(read_skill(&linked.work.0, "pod-watch", &cfg.skill_folders).is_ok());
    let elsewhere = Linked::new("elsewhere");
    let link = linked.work.0.join(".claude/skills/pod-watch");
    std::fs::remove_file(&link).unwrap();
    symlink(elsewhere.kept.0.join("elsewhere/pod-watch"), &link).unwrap();
    assert_eq!(
        read_skill(&linked.work.0, "pod-watch", &cfg.skill_folders),
        Err(SkillMissing)
    );
}

fn add_dirs(line: &str, home: Option<&str>) -> Vec<String> {
    parse_skill(&format!("---\n{line}\n---\nB\n"), home).add_dirs
}

#[test]
fn add_dirs_are_split_trimmed_and_tilde_expanded() {
    let text = "---\nadd-dirs: ~/bin/harold, /tmp/x , ,rel\n---\nB\n";
    let skill = parse_skill(text, Some("/h"));
    assert_eq!(skill.add_dirs, ["/h/bin/harold", "/tmp/x", "rel"]);
    assert_eq!(skill.body, "B\n");
}

#[test]
fn a_bare_tilde_is_home_and_a_folder_with_spaces_stays_whole() {
    assert_eq!(add_dirs("add-dirs: ~", Some("/h")), ["/h"]);
    assert_eq!(add_dirs("add-dirs: /a b/c", Some("/h")), ["/a b/c"]);
}

#[test]
fn no_key_or_an_empty_value_gives_no_add_dirs() {
    assert!(add_dirs("name: x", Some("/h")).is_empty());
    assert!(add_dirs("add-dirs:", Some("/h")).is_empty());
    assert!(add_dirs("add-dirs:  , ,", Some("/h")).is_empty());
    assert!(parse_skill("Just text\n", Some("/h")).add_dirs.is_empty());
}

#[test]
fn only_a_leading_tilde_or_tilde_slash_expands() {
    assert_eq!(
        add_dirs("add-dirs: ~bob/x, a~/b, ~/c", Some("/h")),
        ["~bob/x", "a~/b", "/h/c"]
    );
}

#[test]
fn tilde_entries_pass_as_written_without_home() {
    assert_eq!(add_dirs("add-dirs: ~, ~/x, /y", None), ["~", "~/x", "/y"]);
}

#[test]
fn add_dirs_in_frontmatter_that_never_closes_are_not_read() {
    assert!(
        parse_skill("---\nadd-dirs: /x\nno end\n", Some("/h"))
            .add_dirs
            .is_empty()
    );
}

#[test]
fn read_skill_reads_add_dirs() {
    let temp = Temp::with_skill(
        "pod-watch",
        "---\nadd-dirs: /x\nallowed-tools: Write\n---\nB\n",
    );
    let skill = read_skill(&temp.0, "pod-watch", &[]).unwrap();
    assert_eq!(skill.add_dirs, ["/x"]);
    assert_eq!(skill.allowed_tools.as_deref(), Some("Write"));
}
