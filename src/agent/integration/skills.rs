//! Embedded Kumu skills management (`~/.kumu/skills/` + per-agent symlinks).

use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};

use super::hooks::temporary_path;
use super::{Status, is_under_managed, non_empty_env_path};

pub const SKILLS: [(&str, &str); 2] = [
    (
        "kumu-panes",
        include_str!("../../../skills/kumu-panes/SKILL.md"),
    ),
    (
        "kumu-artifacts",
        include_str!("../../../skills/kumu-artifacts/SKILL.md"),
    ),
];

fn home_dir() -> io::Result<PathBuf> {
    crate::db::db_path()
        .parent()
        .and_then(Path::parent)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("无法确定用户主目录（HOME 未设置）"))
}

pub(super) fn managed_skills_dir(home: &Path) -> PathBuf {
    home.join(".kumu/skills")
}

/// Resolve the user-level skills directory for `agent`.
///
/// Note: Codex reads `$HOME/.agents/skills` (unaffected by `CODEX_HOME`), which
/// is also scanned by several other agents; Antigravity CLI shares
/// `~/.gemini/config/skills` with Antigravity App/IDE (matching `hooks.json`).
pub(super) fn agent_skills_dir<F>(agent: &str, home: &Path, env: F) -> io::Result<PathBuf>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let dir = match agent {
        "codex" => home.join(".agents/skills"),
        "agy" => home.join(".gemini/config/skills"),
        "claude" => env("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|| home.join(".claude"))
            .join("skills"),
        "cursor" => env("CURSOR_CONFIG_DIR")
            .or_else(|| env("XDG_CONFIG_HOME").map(|p| p.join("cursor")))
            .unwrap_or_else(|| home.join(".cursor"))
            .join("skills"),
        "opencode" => env("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode/skills"),
        "pi" => env("PI_CODING_AGENT_DIR")
            .unwrap_or_else(|| home.join(".pi/agent"))
            .join("skills"),
        "omp" => env("PI_CODING_AGENT_DIR")
            .unwrap_or_else(|| {
                env("PI_CONFIG_DIR")
                    .unwrap_or_else(|| home.join(".omp"))
                    .join("agent")
            })
            .join("skills"),
        "grok" => home.join(".grok/skills"),
        _ => return Err(io::Error::other("不支持的 Agent")),
    };
    Ok(dir)
}

pub fn skills_status(agent: &str) -> io::Result<Status> {
    let home = home_dir()?;
    skills_status_in(agent, &home, non_empty_env_path)
}

pub fn install_skills(agent: &str) -> io::Result<()> {
    let home = home_dir()?;
    install_skills_in(agent, &home, non_empty_env_path)
}

pub fn uninstall_skills(agent: &str, force_shared: bool) -> io::Result<()> {
    let home = home_dir()?;
    let my_dir = agent_skills_dir(agent, &home, non_empty_env_path)?;
    if !force_shared && shared_dir_still_in_use(agent, &my_dir, &home) {
        return Ok(());
    }
    uninstall_skills_in(agent, &home, non_empty_env_path)
}

/// Refresh `~/.kumu/skills/` at startup only when it already exists, so
/// existing users who never installed skills are never modified silently.
pub fn refresh_skills() {
    let Ok(home) = home_dir() else {
        return;
    };
    let managed = managed_skills_dir(&home);
    if !managed.exists() {
        return;
    }
    if let Err(error) = write_managed_skills(&managed) {
        eprintln!("[agent] 刷新托管 skills 失败：{error}");
    }
}

fn shared_dir_still_in_use(removing: &str, dir: &Path, home: &Path) -> bool {
    crate::agent::AGENTS.iter().any(|other| {
        if other.id == removing {
            return false;
        }
        let Ok(other_dir) = agent_skills_dir(other.id, home, non_empty_env_path) else {
            return false;
        };
        if other_dir != dir {
            return false;
        }
        // Another agent sharing this exact skills directory still has Kumu
        // integration enabled; keep the shared symlinks until it is also removed.
        !matches!(super::notify_status(other.id), Ok(Status::Missing))
            || (super::supports_mcp(other.id)
                && !matches!(super::mcp_status(other.id), Ok(Status::Missing)))
    })
}

pub(super) fn skills_status_in<F>(agent: &str, home: &Path, env: F) -> io::Result<Status>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let managed = managed_skills_dir(home);
    let skills_dir = agent_skills_dir(agent, home, env)?;
    let mut missing = 0usize;
    let mut registered = 0usize;

    for (name, expected) in SKILLS {
        let entry = skills_dir.join(name);
        let target = managed.join(name);
        match std::fs::symlink_metadata(&entry) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing += 1;
            }
            Err(error) => return Err(error),
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(&entry) {
                Ok(current) if current == target => {
                    let up_to_date = std::fs::read_to_string(target.join("SKILL.md"))
                        .ok()
                        .as_deref()
                        == Some(expected);
                    if up_to_date {
                        registered += 1;
                    }
                }
                Ok(current) if is_under_managed(&current, &managed) => {}
                Ok(_) if is_kumu_legacy_skill_copy(&entry, name) => {}
                Ok(_) => {
                    return Err(io::Error::other(format!(
                        "Skill 路径已被其他链接占用：{}",
                        entry.display()
                    )));
                }
                Err(error) => return Err(error),
            },
            Ok(_) => {
                if !is_kumu_legacy_skill_copy(&entry, name) {
                    return Err(io::Error::other(format!(
                        "Skill 路径已有非 Kumu 文件，未覆盖：{}",
                        entry.display()
                    )));
                }
            }
        }
    }

    if missing == SKILLS.len() {
        Ok(Status::Missing)
    } else if registered == SKILLS.len() {
        Ok(Status::Registered)
    } else {
        Ok(Status::NeedsRepair)
    }
}

pub(super) fn install_skills_in<F>(agent: &str, home: &Path, env: F) -> io::Result<()>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let managed = managed_skills_dir(home);
    let skills_dir = agent_skills_dir(agent, home, env)?;
    write_managed_skills(&managed)?;
    std::fs::create_dir_all(&skills_dir)?;

    for (name, _) in SKILLS {
        let entry = skills_dir.join(name);
        let target = managed.join(name);
        match std::fs::symlink_metadata(&entry) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&entry).ok().as_deref() == Some(target.as_path()) {
                    continue;
                }
                let ours = std::fs::read_link(&entry)
                    .ok()
                    .is_some_and(|p| is_under_managed(&p, &managed));
                if !ours && !is_kumu_legacy_skill_copy(&entry, name) {
                    return Err(io::Error::other(format!(
                        "Skill 路径已被其他链接占用，未覆盖：{}",
                        entry.display()
                    )));
                }
            }
            Ok(meta) => {
                if !is_kumu_legacy_skill_copy(&entry, name) {
                    return Err(io::Error::other(format!(
                        "Skill 路径已有非 Kumu 文件，未覆盖：{}",
                        entry.display()
                    )));
                }
                // Migrate legacy `npx skills add yankewei/kumu` directory copy.
                if meta.is_dir() {
                    std::fs::remove_dir_all(&entry)?;
                } else {
                    std::fs::remove_file(&entry)?;
                }
            }
        }
        let temporary = temporary_path(&entry);
        symlink(&target, &temporary)?;
        if let Err(error) = std::fs::rename(&temporary, &entry) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    }
    Ok(())
}

pub(super) fn uninstall_skills_in<F>(agent: &str, home: &Path, env: F) -> io::Result<()>
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let managed = managed_skills_dir(home);
    let skills_dir = agent_skills_dir(agent, home, env)?;
    for (name, _) in SKILLS {
        let entry = skills_dir.join(name);
        let target = managed.join(name);
        match std::fs::symlink_metadata(&entry) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
            Ok(meta) if !meta.file_type().is_symlink() => continue,
            Ok(_) => {}
        }
        match std::fs::read_link(&entry) {
            Ok(current) if current == target || is_under_managed(&current, &managed) => {
                std::fs::remove_file(&entry)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_managed_skills(managed: &Path) -> io::Result<()> {
    for (name, contents) in SKILLS {
        let file = managed.join(name).join("SKILL.md");
        let parent = file.parent().expect("skill directory");
        std::fs::create_dir_all(parent)?;
        if std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(io::Error::other(format!(
                "托管 Skill 文件不应为符号链接：{}",
                file.display()
            )));
        }
        if std::fs::read_to_string(&file).ok().as_deref() == Some(contents) {
            continue;
        }
        let temporary = temporary_path(&file);
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)?;
        let result = (|| {
            out.write_all(contents.as_bytes())?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&temporary, &file)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
    }
    Ok(())
}

/// Detect whether `path` is a legacy Kumu skill copy (e.g. installed earlier
/// via `npx skills add yankewei/kumu`) so installation can safely migrate it to
/// a managed symlink without ever touching unrelated user files.
pub(super) fn is_kumu_legacy_skill_copy(path: &Path, skill_name: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path.join("SKILL.md")) else {
        return false;
    };
    let Some(frontmatter) = extract_frontmatter(&text) else {
        return false;
    };
    let name_matches = frontmatter.lines().any(|line| {
        let Some(rest) = line.trim().strip_prefix("name:") else {
            return false;
        };
        let value = rest.trim().trim_matches('"').trim_matches('\'');
        value == skill_name
    });
    if !name_matches {
        return false;
    }
    text.contains("KUMU_APP_BINARY")
        || text.contains("kumu pane")
        || text.contains("kumu artifact")
        || text.contains("artifact_publish")
        || text.contains("pane_read")
}

fn extract_frontmatter(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some(&rest[..offset]);
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kumu-skills-{tag}-{}-{nonce}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp home");
        dir
    }

    fn no_env(_: &str) -> Option<PathBuf> {
        None
    }

    #[test]
    fn agent_skills_paths_match_official_locations() {
        let home = Path::new("/Users/tester");
        assert_eq!(
            agent_skills_dir("codex", home, |_| Some(PathBuf::from("/custom/codex"))).unwrap(),
            PathBuf::from("/Users/tester/.agents/skills"),
            "codex user skills live under $HOME/.agents/skills regardless of CODEX_HOME"
        );
        assert_eq!(
            agent_skills_dir("agy", home, no_env).unwrap(),
            PathBuf::from("/Users/tester/.gemini/config/skills")
        );
        assert_eq!(
            agent_skills_dir("claude", home, no_env).unwrap(),
            PathBuf::from("/Users/tester/.claude/skills")
        );
        assert_eq!(
            agent_skills_dir("opencode", home, no_env).unwrap(),
            PathBuf::from("/Users/tester/.config/opencode/skills")
        );
    }

    #[test]
    fn install_status_stale_and_uninstall_round_trip() {
        let home = temp_home("roundtrip");
        assert_eq!(
            skills_status_in("claude", &home, no_env).unwrap(),
            Status::Missing
        );

        install_skills_in("claude", &home, no_env).unwrap();
        assert_eq!(
            skills_status_in("claude", &home, no_env).unwrap(),
            Status::Registered
        );

        // Stale managed file triggers NeedsRepair and install repairs it.
        std::fs::write(managed_skills_dir(&home).join("kumu-panes/SKILL.md"), "old").unwrap();
        assert_eq!(
            skills_status_in("claude", &home, no_env).unwrap(),
            Status::NeedsRepair
        );
        install_skills_in("claude", &home, no_env).unwrap();
        assert_eq!(
            skills_status_in("claude", &home, no_env).unwrap(),
            Status::Registered
        );

        // Unrelated user skill survives uninstall.
        let user_skill = home.join(".claude/skills/my-own-skill");
        std::fs::create_dir_all(&user_skill).unwrap();
        uninstall_skills_in("claude", &home, no_env).unwrap();
        assert_eq!(
            skills_status_in("claude", &home, no_env).unwrap(),
            Status::Missing
        );
        assert!(user_skill.exists());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn migrates_legacy_kumu_copy_and_refuses_foreign_directory() {
        let home = temp_home("legacy");
        let claude_skills = home.join(".claude/skills");

        // Legacy `npx skills add yankewei/kumu` copy is recognized and migrated.
        let legacy_dir = claude_skills.join("kumu-panes");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join("SKILL.md"),
            "---\nname: kumu-panes\n---\nUse $KUMU_APP_BINARY panes.\n",
        )
        .unwrap();
        assert_eq!(
            skills_status_in("claude", &home, no_env).unwrap(),
            Status::NeedsRepair
        );
        install_skills_in("claude", &home, no_env).unwrap();
        assert_eq!(
            skills_status_in("claude", &home, no_env).unwrap(),
            Status::Registered
        );
        assert!(
            std::fs::symlink_metadata(&legacy_dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // Unrelated user directory with same name is never overwritten.
        let foreign_dir = home.join(".cursor/skills/kumu-panes");
        std::fs::create_dir_all(&foreign_dir).unwrap();
        std::fs::write(foreign_dir.join("SKILL.md"), "# My custom notes\n").unwrap();
        assert!(skills_status_in("cursor", &home, no_env).is_err());
        assert!(install_skills_in("cursor", &home, no_env).is_err());
        assert!(foreign_dir.join("SKILL.md").exists());

        let _ = std::fs::remove_dir_all(&home);
    }
}
