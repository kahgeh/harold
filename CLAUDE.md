# Superpowers artifacts

Save all superpowers output under `.superpowers/` at the repo root, not `docs/superpowers/`. This overrides the default paths in the superpowers skills.

- Brainstorming and design specs: `.superpowers/specs/YYYY-MM-DD-<topic>-design.md`
- Implementation plans: `.superpowers/plans/YYYY-MM-DD-<feature-name>.md`
- Visual companion mockups (`--project-dir` set to the repo root): `.superpowers/brainstorm/`
- Subagent-driven execution workspaces: `.superpowers/sdd/<plan-basename>/`

`.superpowers/` is git-ignored as a whole: nothing in it is committed, so do not `git add` specs or plans. This overrides the skills' "commit the spec" step.
