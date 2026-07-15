//! `setup` — make a repo testsprite-rs-aware for coding agents, with no cloud
//! and no account.
//!
//! Installs the two agent-facing skills (onboard + verify) into the target
//! repo's `.claude/skills/<name>/SKILL.md` so an agent working there discovers
//! how to seed and verify a local test suite. The skill bodies are embedded in
//! the binary (`include_str!`), so an install always matches this build — no
//! dependency on the source tree being present.
//!
//! Note: this does NOT copy testsprite-rs's own `AGENTS.md` — that documents how
//! to develop testsprite-rs itself and would be wrong content in a target repo.
//! The skills are the agent guidance; a one-line pointer for the repo's own
//! AGENTS.md is printed instead.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// One installable skill: its directory name, a trigger description (skills need
/// YAML frontmatter for discovery; the bodies ship without it), and its body.
struct Skill {
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

const SKILLS: &[Skill] = &[
    Skill {
        name: "testsprite-onboard",
        description: "Onboard a repo that has no testsprite-rs tests yet with a runnable seed \
                      suite (local, no cloud). Use when first setting up testsprite-rs testing \
                      for a project.",
        body: include_str!("../../skills/testsprite-onboard.md"),
    },
    Skill {
        name: "testsprite-verify",
        description: "testsprite-rs verification loop (local, no cloud): after a change, run the \
                      affected tests to a terminal verdict before calling it done. Use when \
                      wrapping up any code change in a testsprite-rs-tested repo.",
        body: include_str!("../../skills/testsprite-verify.md"),
    },
];

/// What happened to one skill file during [`install`].
pub struct Installed {
    pub path: PathBuf,
    /// `false` when the file already existed and `force` was not set (skipped).
    pub written: bool,
}

/// Install the agent skills under `root/.claude/skills`. Existing files are left
/// untouched (reported as skipped) unless `force` is set — never clobber a
/// repo's customized skill without being asked.
pub fn install(root: &Path, force: bool) -> anyhow::Result<Vec<Installed>> {
    let mut out = Vec::with_capacity(SKILLS.len());
    for skill in SKILLS {
        let dir = root.join(".claude").join("skills").join(skill.name);
        let file = dir.join("SKILL.md");
        if file.exists() && !force {
            out.push(Installed {
                path: file,
                written: false,
            });
            continue;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // The description is emitted as a double-quoted YAML scalar: it contains
        // `: ` (a mapping indicator) and could contain `#`, so an unquoted value
        // would be invalid frontmatter and the skill would fail to load.
        let content = format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}",
            skill.name,
            yaml_quote(skill.description),
            skill.body.trim_start()
        );
        std::fs::write(&file, content).with_context(|| format!("writing {}", file.display()))?;
        out.push(Installed {
            path: file,
            written: true,
        });
    }
    Ok(out)
}

/// Escape `s` as a YAML double-quoted scalar, so a value containing `:`, `#`,
/// etc. stays valid frontmatter.
fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Run `setup`: install the skills, print what happened + next steps. Returns
/// the process exit code (0).
pub fn setup(root: &Path, force: bool) -> anyhow::Result<i32> {
    let installed = install(root, force)?;
    let (written, skipped): (Vec<_>, Vec<_>) = installed.iter().partition(|i| i.written);

    for i in &written {
        println!("installed  {}", i.path.display());
    }
    for i in &skipped {
        println!("exists     {} (use --force to overwrite)", i.path.display());
    }

    println!();
    println!("testsprite-rs is set up for agents in this repo. Next:");
    println!("  1. testsprite-rs project init --type backend --name <app> --url <url>");
    println!("  2. testsprite-rs test generate --doc <openapi|postman|har>   # or --instruction");
    println!(
        "  3. testsprite-rs loop --json                                  # run → triage → surface"
    );
    println!();
    println!("Point your agent at the installed skills, or add to AGENTS.md:");
    println!("  > Test with testsprite-rs; see .claude/skills/testsprite-verify.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_both_skills_with_frontmatter() {
        let root = crate::local::tmp_root();
        let installed = install(&root, false).unwrap();
        assert_eq!(installed.len(), 2);
        assert!(installed.iter().all(|i| i.written));

        let onboard = root.join(".claude/skills/testsprite-onboard/SKILL.md");
        let verify = root.join(".claude/skills/testsprite-verify/SKILL.md");
        assert!(onboard.exists() && verify.exists());

        let text = std::fs::read_to_string(&verify).unwrap();
        assert!(text.starts_with("---\n"), "{text:.40}");

        // The frontmatter must be VALID YAML — the verify description contains a
        // `: ` that, emitted unquoted, made a spec parser reject the whole block
        // (so Claude Code couldn't load the skill). Parse it for real.
        let after = text.strip_prefix("---\n").expect("frontmatter start");
        let fm = after.split("\n---\n").next().expect("frontmatter end");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(fm).expect("frontmatter is valid YAML");
        assert_eq!(parsed["name"].as_str(), Some("testsprite-verify"));
        assert!(
            parsed["description"]
                .as_str()
                .unwrap()
                .contains("verification loop")
        );
        // …and the real body followed the frontmatter.
        assert!(text.contains("\n---\n\n"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_does_not_clobber_without_force() {
        let root = crate::local::tmp_root();
        let dir = root.join(".claude/skills/testsprite-verify");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("SKILL.md");
        std::fs::write(&file, "MY CUSTOM SKILL").unwrap();

        // Without --force the customized file is preserved and reported skipped.
        let installed = install(&root, false).unwrap();
        let verify = installed.iter().find(|i| i.path == file).unwrap();
        assert!(!verify.written);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "MY CUSTOM SKILL");

        // With --force it is overwritten.
        let installed = install(&root, true).unwrap();
        assert!(installed.iter().find(|i| i.path == file).unwrap().written);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("testsprite-verify")
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
