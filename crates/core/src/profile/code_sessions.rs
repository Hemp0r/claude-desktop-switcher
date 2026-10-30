//! The Claude Desktop App's Code-tab conversation list (SPECIFICATION.md §3
//! 「Codeタブの会話一覧」).
//!
//! The Desktop app lists the Claude Code conversations of its Code tab from an
//! index kept inside its `--user-data-dir`, in a folder keyed by the signed-in
//! account and organization:
//!
//! ```text
//! <desktop-data>/claude-code-sessions/<accountUuid>/<organizationUuid>/local_<id>.json
//! ```
//!
//! The list is never shared. The Desktop app does not write through links in
//! its data folders: it opens the list folder with `O_DIRECTORY | O_NOFOLLOW`,
//! so on a link every save fails with `ENOTDIR`. Reads still go through, so a
//! linked list shows up, but conversations started in that environment are
//! kept only in memory and are lost when the app quits.
//!
//! v0.24.8 linked the list in "account only" environments and kept the
//! environment's own list beside the link under `<org>.csw-local` (then
//! `.csw-local-2`, …). This module undoes that: on launch and with
//! `csw doctor --fix` it removes the links and renames the newest kept list
//! back.
//!
//! Scan scope (docs/PRIVACY.md): only the names and types of the two folder
//! levels under the environment's `claude-code-sessions/` are listed; file
//! contents are never opened. Nothing is read or written on the existing
//! Claude side.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{CswError, Result};
use crate::platform::PlatformProvider;
use crate::profile::Profile;
use crate::profile::inspector::ItemHealth;
use crate::switcher::desktop_dir_running;

/// Item key in the isolation report (after the 12 `LINK_ITEMS`).
pub const KEY: &str = "desktop_code_sessions";

/// The index folder inside a Desktop data dir.
pub const INDEX_DIR: &str = "claude-code-sessions";

/// Suffix v0.24.8 kept the environment's own list under. It is not a UUID, so
/// it is never counted as an account folder.
pub const LOCAL_SUFFIX: &str = ".csw-local";

/// Classify the item for the isolation check. The list is always isolated:
/// any link in the environment's index is an issue that `restore` repairs.
pub fn inspect(profile: &Profile, provider: &dyn PlatformProvider) -> ItemHealth {
    match links(&profile.isolation.desktop_user_data_dir, provider).first() {
        Some(link) => ItemHealth::UnexpectedLink {
            actual_target: display(&fs::read_link(link).unwrap_or_default()),
        },
        None => ItemHealth::IsolatedOk,
    }
}

/// Undo v0.24.8's link in the environment's index: remove every link (an
/// account folder entry, or the index folder itself; their targets are never
/// touched), then rename the newest kept list back wherever nothing is left in
/// its place. A real folder already in place is never touched. Returns
/// whether anything changed. The caller makes sure the environment's Claude is
/// not running.
pub fn restore(profile: &Profile, provider: &dyn PlatformProvider) -> Result<bool> {
    let env_dir = &profile.isolation.desktop_user_data_dir;
    let mut changed = false;
    for link in links(env_dir, provider) {
        assert_outside_default_roots(&link, provider)?;
        provider.remove_symlink(&link)?;
        changed = true;
    }
    for account in real_dirs(uuid_entries(&env_dir.join(INDEX_DIR))) {
        for (place, kept) in newest_kept(&account) {
            if fs::symlink_metadata(&place).is_ok() {
                continue;
            }
            assert_outside_default_roots(&place, provider)?;
            fs::rename(&kept, &place)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Launch-time hook (`switcher::desktop::launch_desktop`): restore the list of
/// a non-default environment whose Claude is not running. Best effort: an
/// error is returned for the caller to ignore.
pub fn prepare_launch(profile: &Profile, provider: &dyn PlatformProvider) -> Result<bool> {
    if profile.profile.is_default {
        return Ok(false);
    }
    let running = provider.running_desktop_args()?;
    if desktop_dir_running(
        &profile.isolation.desktop_user_data_dir,
        &running,
        &provider.claude_desktop_default_dir(),
    ) {
        return Ok(false);
    }
    restore(profile, provider)
}

/// Every link in `desktop_dir`'s index, sorted: the index folder itself when
/// it is one (nothing behind it is walked), otherwise the UUID-named entries
/// under its real account folders. Only names and types are read.
fn links(desktop_dir: &Path, provider: &dyn PlatformProvider) -> Vec<PathBuf> {
    let index = desktop_dir.join(INDEX_DIR);
    if provider.is_symlink(&index) {
        return vec![index];
    }
    let mut links: Vec<PathBuf> = real_dirs(uuid_entries(&index))
        .into_iter()
        .flat_map(|account| uuid_entries(&account))
        .filter(|entry| provider.is_symlink(entry))
        .collect();
    links.sort();
    links
}

/// For each organization folder kept under `account` by v0.24.8, the place to
/// restore it and the newest kept list: `<org>.csw-local` counts as 1 and
/// `<org>.csw-local-<n>` as n, the highest number being the last one kept.
fn newest_kept(account: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = fs::read_dir(account) else {
        return Vec::new();
    };
    let mut newest: BTreeMap<String, (u32, PathBuf)> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some((org, n)) = entry.file_name().to_str().and_then(kept_name) else {
            continue;
        };
        if !is_real_dir(&path) || newest.get(&org).is_some_and(|(m, _)| *m >= n) {
            continue;
        }
        newest.insert(org, (n, path));
    }
    newest
        .into_iter()
        .map(|(org, (_, kept))| (account.join(org), kept))
        .collect()
}

/// `<uuid>.csw-local` → (uuid, 1); `<uuid>.csw-local-<n>` with n ≥ 2 → (uuid, n).
fn kept_name(name: &str) -> Option<(String, u32)> {
    let (org, rest) = name.split_once(LOCAL_SUFFIX)?;
    if !is_uuid(org) {
        return None;
    }
    let n = match rest {
        "" => 1,
        _ => rest.strip_prefix('-')?.parse().ok().filter(|n| *n >= 2)?,
    };
    Some((org.to_string(), n))
}

fn real_dirs(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| is_real_dir(p)).collect()
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Entries of `dir` whose names are UUIDs. Names only; nothing is opened.
fn uuid_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(is_uuid))
        .map(|entry| entry.path())
        .collect()
}

fn is_uuid(name: &str) -> bool {
    name.len() == 36
        && name.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Defense in depth, mirroring the linker's guard: never move or unlink inside
/// the user's real default Claude data dirs. Both the literal path and where
/// its parent folder really is are checked: a parent reached through a
/// symlink would pass a textual comparison alone.
fn assert_outside_default_roots(path: &Path, provider: &dyn PlatformProvider) -> Result<()> {
    let defaults = [
        provider.claude_desktop_default_dir(),
        provider.claude_cli_default_dir(),
    ];
    let mut candidates = vec![path.to_path_buf()];
    if let Some(real_parent) = path.parent().and_then(|p| p.canonicalize().ok()) {
        candidates.push(match path.file_name() {
            Some(name) => real_parent.join(name),
            None => real_parent,
        });
    }
    for root in &defaults {
        let real_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        if candidates
            .iter()
            .any(|c| c.starts_with(root) || c.starts_with(&real_root))
        {
            return Err(CswError::RefusedDefaultDataDeletion(
                path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mock::MockPlatformProvider;
    use crate::profile::{IsolationConfig, ProfileMeta, SharingConfig};
    use tempfile::{TempDir, tempdir};

    const ACCT_A: &str = "11111111-1111-4111-8111-111111111111";
    const ORG_A: &str = "22222222-2222-4222-8222-222222222222";
    const ACCT_B: &str = "33333333-3333-4333-8333-333333333333";
    const ORG_B: &str = "44444444-4444-4444-8444-444444444444";

    struct Fixture {
        tmp: TempDir,
        provider: MockPlatformProvider,
        default_desktop: PathBuf,
        profile: Profile,
    }

    fn fixture() -> Fixture {
        let tmp = tempdir().unwrap();
        let default_desktop = tmp.path().join("desktop_default");
        let cli_default = tmp.path().join("cli_default");
        let env = tmp.path().join("profiles").join("work");
        fs::create_dir_all(&default_desktop).unwrap();
        fs::create_dir_all(&cli_default).unwrap();
        fs::create_dir_all(env.join("desktop-data")).unwrap();
        let provider =
            MockPlatformProvider::new(default_desktop.clone(), cli_default, tmp.path().join("app"));
        let profile = Profile {
            profile: ProfileMeta {
                name: "work".to_string(),
                icon: String::new(),
                color: String::new(),
                is_default: false,
                note: String::new(),
                created_at: None,
                cloned_from: None,
            },
            isolation: IsolationConfig {
                desktop_user_data_dir: env.join("desktop-data"),
                cli_config_dir: env.join("cli-data"),
            },
            sharing: SharingConfig::share_workspace_preset(),
        };
        Fixture {
            tmp,
            provider,
            default_desktop,
            profile,
        }
    }

    /// Create `<desktop>/claude-code-sessions/<acct>/<org>/` with one entry.
    fn account_dir(desktop: &Path, acct: &str, org: &str, entry: &str) -> PathBuf {
        let dir = desktop.join(INDEX_DIR).join(acct).join(org);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(entry), "{}").unwrap();
        dir
    }

    fn env_desktop(f: &Fixture) -> PathBuf {
        f.profile.isolation.desktop_user_data_dir.clone()
    }

    /// The environment's account folder path, `<org>` under `<acct>`.
    fn own_path(f: &Fixture) -> PathBuf {
        env_desktop(f).join(INDEX_DIR).join(ACCT_B).join(ORG_B)
    }

    fn kept(own: &Path, suffix: &str) -> PathBuf {
        own.with_file_name(format!("{ORG_B}{LOCAL_SUFFIX}{suffix}"))
    }

    /// v0.24.8's state: the account folder is a link to the existing Claude's
    /// list and the own list sits at `<org>.csw-local`. Returns the source.
    fn linked(f: &Fixture) -> PathBuf {
        let source = account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(f), ACCT_B, ORG_B, "local_b.json");
        fs::rename(&own, kept(&own, "")).unwrap();
        std::os::unix::fs::symlink(&source, &own).unwrap();
        source
    }

    #[test]
    fn restore_removes_the_link_and_brings_the_own_list_back() {
        let f = fixture();
        let source = linked(&f);
        let own = own_path(&f);

        assert!(restore(&f.profile, &f.provider).unwrap());

        assert!(!f.provider.is_symlink(&own));
        assert!(own.join("local_b.json").exists());
        assert!(!kept(&own, "").exists());
        // The existing Claude's list is untouched.
        assert!(source.join("local_a.json").exists());
        assert!(!source.join("local_b.json").exists());

        // Idempotent.
        assert!(!restore(&f.profile, &f.provider).unwrap());
        assert!(own.join("local_b.json").exists());
    }

    #[test]
    fn restore_takes_the_newest_kept_list() {
        let f = fixture();
        linked(&f);
        let own = own_path(&f);
        for (suffix, entry) in [("-2", "local_2.json"), ("-10", "local_10.json")] {
            fs::create_dir_all(kept(&own, suffix)).unwrap();
            fs::write(kept(&own, suffix).join(entry), "{}").unwrap();
        }

        assert!(restore(&f.profile, &f.provider).unwrap());

        assert!(own.join("local_10.json").exists(), "highest number wins");
        assert!(kept(&own, "").join("local_b.json").exists());
        assert!(kept(&own, "-2").join("local_2.json").exists());
        assert!(!kept(&own, "-10").exists());
    }

    #[test]
    fn restore_without_a_kept_list_only_removes_the_link() {
        let f = fixture();
        let source = linked(&f);
        let own = own_path(&f);
        fs::remove_dir_all(kept(&own, "")).unwrap();

        assert!(restore(&f.profile, &f.provider).unwrap());

        assert!(fs::symlink_metadata(&own).is_err(), "nothing left in place");
        assert!(source.join("local_a.json").exists());
    }

    #[test]
    fn restore_removes_a_dangling_link() {
        let f = fixture();
        let own = own_path(&f);
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(f.tmp.path().join("gone"), &own).unwrap();

        assert!(restore(&f.profile, &f.provider).unwrap());
        assert!(fs::symlink_metadata(&own).is_err());
    }

    #[test]
    fn restore_leaves_a_real_folder_and_the_kept_list_alone() {
        let f = fixture();
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_new.json");
        fs::create_dir_all(kept(&own, "")).unwrap();
        fs::write(kept(&own, "").join("local_b.json"), "{}").unwrap();

        assert!(!restore(&f.profile, &f.provider).unwrap());

        assert!(own.join("local_new.json").exists());
        assert!(kept(&own, "").join("local_b.json").exists());
    }

    #[test]
    fn restore_brings_back_a_kept_list_with_nothing_in_place() {
        // A duplicate: the link was skipped, the kept list was copied.
        let f = fixture();
        let own = own_path(&f);
        fs::create_dir_all(kept(&own, "")).unwrap();
        fs::write(kept(&own, "").join("local_b.json"), "{}").unwrap();

        assert!(restore(&f.profile, &f.provider).unwrap());
        assert!(own.join("local_b.json").exists());
    }

    #[test]
    fn restore_removes_an_index_folder_link_but_not_its_target() {
        let f = fixture();
        let source = account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let index = env_desktop(&f).join(INDEX_DIR);
        std::os::unix::fs::symlink(f.default_desktop.join(INDEX_DIR), &index).unwrap();

        assert!(restore(&f.profile, &f.provider).unwrap());

        assert!(fs::symlink_metadata(&index).is_err());
        assert!(source.join("local_a.json").exists());
    }

    #[test]
    fn restore_ignores_names_that_are_not_uuids() {
        let f = fixture();
        let stray = env_desktop(&f).join(INDEX_DIR).join(ACCT_B).join("backlog");
        fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(f.tmp.path(), &stray).unwrap();

        assert!(!restore(&f.profile, &f.provider).unwrap());
        assert!(f.provider.is_symlink(&stray));
    }

    #[test]
    fn restore_refuses_to_operate_inside_the_default_dir() {
        // A misconfigured environment whose Desktop dir IS the default dir.
        let mut f = fixture();
        f.profile.isolation.desktop_user_data_dir = f.default_desktop.clone();
        let link = f.default_desktop.join(INDEX_DIR).join(ACCT_A).join(ORG_B);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(f.tmp.path(), &link).unwrap();

        assert!(restore(&f.profile, &f.provider).is_err());
        assert!(f.provider.is_symlink(&link));
    }

    #[test]
    fn inspect_reports_links_as_issues() {
        let f = fixture();
        assert_eq!(inspect(&f.profile, &f.provider), ItemHealth::IsolatedOk);

        account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        assert_eq!(inspect(&f.profile, &f.provider), ItemHealth::IsolatedOk);

        fs::remove_dir_all(env_desktop(&f).join(INDEX_DIR)).unwrap();
        let source = linked(&f);
        let health = inspect(&f.profile, &f.provider);
        assert_eq!(
            health,
            ItemHealth::UnexpectedLink {
                actual_target: source.display().to_string()
            }
        );
        assert!(health.is_issue());

        // The index folder itself as a link.
        let f = fixture();
        let target = f.default_desktop.join(INDEX_DIR);
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, env_desktop(&f).join(INDEX_DIR)).unwrap();
        assert_eq!(
            inspect(&f.profile, &f.provider),
            ItemHealth::UnexpectedLink {
                actual_target: target.display().to_string()
            }
        );
    }

    #[test]
    fn prepare_launch_restores_unless_the_environment_runs() {
        let f = fixture();
        linked(&f);
        let own = own_path(&f);

        let running = MockPlatformProvider::new(
            f.default_desktop.clone(),
            f.tmp.path().join("cli_default"),
            f.tmp.path().join("app"),
        )
        .with_running_args(vec![format!(
            "/Applications/Claude.app/Contents/MacOS/Claude --user-data-dir={}",
            env_desktop(&f).display()
        )]);
        assert!(!prepare_launch(&f.profile, &running).unwrap());
        assert!(f.provider.is_symlink(&own));

        assert!(prepare_launch(&f.profile, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));
    }

    #[test]
    fn prepare_launch_ignores_the_existing_claude() {
        let mut f = fixture();
        linked(&f);
        f.profile.profile.is_default = true;
        assert!(!prepare_launch(&f.profile, &f.provider).unwrap());
        assert!(f.provider.is_symlink(&own_path(&f)));
    }
}
