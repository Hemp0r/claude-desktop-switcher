//! The Claude Desktop App's Code-tab conversation list (SPECIFICATION.md §3
//! 「Codeタブの会話一覧」, issue #234).
//!
//! The Desktop app lists the Claude Code conversations of its Code tab from an
//! index kept inside its `--user-data-dir`, in a folder keyed by the signed-in
//! account and organization:
//!
//! ```text
//! <desktop-data>/claude-code-sessions/<accountUuid>/<organizationUuid>/local_<id>.json
//! ```
//!
//! The conversations themselves live in the CLI dir's `projects/`, which the
//! linker shares for `cli_project_memory`. Without this index the Code tab of a
//! sharing environment stays empty although the conversations are there.
//!
//! The environment's folder name is only known after its first sign-in, so
//! this item cannot be a static `LINK_ITEMS` entry. Its link is created when
//! CSW launches the environment (and by `csw doctor --fix`), once the existing
//! Claude and the environment each hold exactly one account folder.
//!
//! Scan scope (docs/PRIVACY.md): only the names and types of the two folder
//! levels under `claude-code-sessions/` are listed; file contents are never
//! opened. Nothing is ever written on the existing Claude side.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{CswError, Result};
use crate::platform::PlatformProvider;
use crate::profile::inspector::ItemHealth;
use crate::profile::{Profile, SharingMode};
use crate::switcher::desktop_dir_running;

/// Item key in the isolation report (after the 12 `LINK_ITEMS`).
pub const KEY: &str = "desktop_code_sessions";

/// The index folder inside a Desktop data dir.
pub const INDEX_DIR: &str = "claude-code-sessions";

/// Suffix the environment's own list is kept under when it is replaced by the
/// shared link. It is not a UUID, so it is never counted as an account folder.
pub const LOCAL_SUFFIX: &str = ".csw-local";

/// The mode of the Code-tab conversation list. It is not a choice of its own:
/// the list indexes the conversation history, so it is shared exactly when
/// `cli_project_memory` is. Copy reads as Isolate, because a one-time copy of
/// an account-keyed index would match neither side's conversations afterwards.
pub fn mode(profile: &Profile) -> SharingMode {
    match profile.sharing.cli_project_memory {
        SharingMode::Share => SharingMode::Share,
        SharingMode::Copy | SharingMode::Isolate => SharingMode::Isolate,
    }
}

/// The `<accountUuid>/<organizationUuid>` folders under `desktop_dir`'s index,
/// sorted. Account folders must be real directories; organization entries may
/// be directories or symlinks (the shared link itself). Names that are not
/// UUIDs are ignored. Only names and types are read.
pub fn account_folders(desktop_dir: &Path) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    for account in uuid_entries(&desktop_dir.join(INDEX_DIR)) {
        if !fs::symlink_metadata(&account).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        for org in uuid_entries(&account) {
            if fs::symlink_metadata(&org).is_ok_and(|m| m.is_dir() || m.file_type().is_symlink()) {
                folders.push(org);
            }
        }
    }
    folders.sort();
    folders
}

/// Classify the item for the isolation check. `source_desktop_dir` is the
/// sharing source's Desktop data dir (the existing Claude).
pub fn inspect(
    profile: &Profile,
    source_desktop_dir: &Path,
    provider: &dyn PlatformProvider,
) -> ItemHealth {
    let targets = account_folders(&profile.isolation.desktop_user_data_dir);
    if mode(profile) != SharingMode::Share {
        return match targets.iter().find(|t| provider.is_symlink(t)) {
            Some(link) => ItemHealth::UnexpectedLink {
                actual_target: display(&fs::read_link(link).unwrap_or_default()),
            },
            None => ItemHealth::IsolatedOk,
        };
    }
    let source = match single(account_folders(source_desktop_dir)) {
        Single::None => return ItemHealth::SourceAbsent,
        Single::Many => return ItemHealth::AccountAmbiguous,
        Single::One(source) => source,
    };
    let target = match single(targets) {
        Single::None => {
            return ItemHealth::PendingLaunch {
                account_folder_exists: false,
            };
        }
        Single::Many => return ItemHealth::AccountAmbiguous,
        Single::One(target) => target,
    };
    if !provider.is_symlink(&target) {
        return ItemHealth::PendingLaunch {
            account_folder_exists: true,
        };
    }
    let actual = fs::read_link(&target).unwrap_or_default();
    if actual == source {
        ItemHealth::SharedOk {
            target: display(&actual),
        }
    } else {
        // The source is known and exists, so the link can always be re-pointed.
        ItemHealth::WrongTarget {
            expected_source: display(&source),
            actual_target: display(&actual),
            fixable: true,
        }
    }
}

/// Point the environment's account folder at the existing Claude's, when the
/// item is shared and both sides hold exactly one account folder. A real
/// folder in the way (the environment's own list) is kept under
/// `<name>.csw-local` and restored if the link cannot be created. Returns
/// whether anything changed. The caller makes sure the environment's Claude is
/// not running.
pub fn link(
    profile: &Profile,
    source_desktop_dir: &Path,
    provider: &dyn PlatformProvider,
) -> Result<bool> {
    if mode(profile) != SharingMode::Share {
        return Ok(false);
    }
    let (Single::One(source), Single::One(target)) = (
        single(account_folders(source_desktop_dir)),
        single(account_folders(&profile.isolation.desktop_user_data_dir)),
    ) else {
        return Ok(false);
    };
    assert_outside_default_roots(&target, provider)?;

    if provider.is_symlink(&target) {
        if fs::read_link(&target).is_ok_and(|actual| actual == source) {
            return Ok(false);
        }
        provider.remove_symlink(&target)?;
        provider.create_symlink(&source, &target)?;
        return Ok(true);
    }

    // The environment's own list is moved aside, never deleted. An earlier
    // kept list is never overwritten: leave everything as it is instead.
    let kept = kept_path(&target);
    if fs::symlink_metadata(&kept).is_ok() {
        return Ok(false);
    }
    fs::rename(&target, &kept)?;
    if let Err(e) = provider.create_symlink(&source, &target) {
        let _ = fs::rename(&kept, &target);
        return Err(e);
    }
    Ok(true)
}

/// Recreate the link in a duplicate. Copying skips symlinks, so a duplicate of
/// a linked environment arrives without one. The duplicate starts signed in to
/// the same account, so the link goes at the same relative path.
pub fn relink_after_clone(
    source_env: &Profile,
    target: &Profile,
    source_desktop_dir: &Path,
    provider: &dyn PlatformProvider,
) -> Result<()> {
    if mode(target) != SharingMode::Share {
        return Ok(());
    }
    let source_env_dir = &source_env.isolation.desktop_user_data_dir;
    let (Single::One(linked), Single::One(source)) = (
        single(account_folders(source_env_dir)),
        single(account_folders(source_desktop_dir)),
    ) else {
        return Ok(());
    };
    if !provider.is_symlink(&linked) {
        return Ok(());
    }
    let Ok(relative) = linked.strip_prefix(source_env_dir) else {
        return Ok(());
    };
    let path = target.isolation.desktop_user_data_dir.join(relative);
    if fs::symlink_metadata(&path).is_ok() {
        return Ok(());
    }
    assert_outside_default_roots(&path, provider)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    provider.create_symlink(&source, &path)
}

/// Launch-time hook (`switcher::desktop::launch_desktop`): link the list when
/// the environment shares from the existing Claude and its Claude is not
/// running. Best effort: an error is returned for the caller to ignore.
pub fn prepare_launch(profile: &Profile, provider: &dyn PlatformProvider) -> Result<bool> {
    if profile.profile.is_default
        || profile.sharing.source.profile != "default"
        || mode(profile) != SharingMode::Share
    {
        return Ok(false);
    }
    let default_dir = provider.claude_desktop_default_dir();
    let running = provider.running_desktop_args()?;
    if desktop_dir_running(
        &profile.isolation.desktop_user_data_dir,
        &running,
        &default_dir,
    ) {
        return Ok(false);
    }
    link(profile, &default_dir, provider)
}

/// Exactly one folder, or why not.
enum Single {
    None,
    One(PathBuf),
    Many,
}

fn single(mut folders: Vec<PathBuf>) -> Single {
    match folders.len() {
        0 => Single::None,
        1 => Single::One(folders.remove(0)),
        _ => Single::Many,
    }
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

/// Sibling path the environment's own list is kept under.
fn kept_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(LOCAL_SUFFIX);
    target.with_file_name(name)
}

/// Defense in depth, mirroring the linker's guard: never move or link inside
/// the user's real default Claude data dirs.
fn assert_outside_default_roots(path: &Path, provider: &dyn PlatformProvider) -> Result<()> {
    let defaults = [
        provider.claude_desktop_default_dir(),
        provider.claude_cli_default_dir(),
    ];
    if defaults.iter().any(|root| path.starts_with(root)) {
        return Err(CswError::RefusedDefaultDataDeletion(
            path.display().to_string(),
        ));
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
        _tmp: TempDir,
        provider: MockPlatformProvider,
        default_desktop: PathBuf,
        profile: Profile,
    }

    fn fixture(sharing: SharingConfig) -> Fixture {
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
            sharing,
        };
        Fixture {
            _tmp: tmp,
            provider,
            default_desktop,
            profile,
        }
    }

    fn shared() -> Fixture {
        fixture(SharingConfig::share_workspace_preset())
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

    #[test]
    fn mode_follows_conversation_history() {
        let mut sharing = SharingConfig::share_workspace_preset();
        assert_eq!(mode(&fixture(sharing.clone()).profile), SharingMode::Share);

        sharing.cli_project_memory = SharingMode::Copy;
        assert_eq!(mode(&fixture(sharing).profile), SharingMode::Isolate);

        let settings = fixture(SharingConfig::share_settings_preset());
        assert_eq!(mode(&settings.profile), SharingMode::Isolate);
        assert_eq!(
            mode(&fixture(SharingConfig::default()).profile),
            SharingMode::Isolate
        );
    }

    #[test]
    fn account_folders_lists_only_uuid_pairs() {
        let f = shared();
        let d = f.default_desktop.clone();
        let a = account_dir(&d, ACCT_A, ORG_A, "local_1.json");
        // Noise that must not count: a non-UUID org entry, a stray file at
        // both levels, and a kept-aside list.
        fs::create_dir_all(d.join(INDEX_DIR).join(ACCT_A).join("backlog")).unwrap();
        fs::write(d.join(INDEX_DIR).join("notes.txt"), "").unwrap();
        fs::create_dir_all(
            d.join(INDEX_DIR)
                .join(ACCT_A)
                .join(format!("{ORG_B}{LOCAL_SUFFIX}")),
        )
        .unwrap();
        assert_eq!(account_folders(&d), vec![a.clone()]);

        let b = account_dir(&d, ACCT_B, ORG_B, "local_2.json");
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(account_folders(&d), expected);

        assert!(account_folders(&env_desktop(&f)).is_empty());
    }

    #[test]
    fn link_shares_the_list_and_keeps_the_environment_list() {
        let f = shared();
        let source = account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");

        assert!(link(&f.profile, &f.default_desktop, &f.provider).unwrap());

        assert!(f.provider.is_symlink(&own));
        assert_eq!(fs::read_link(&own).unwrap(), source);
        let kept = own.with_file_name(format!("{ORG_B}{LOCAL_SUFFIX}"));
        assert!(kept.join("local_b.json").exists(), "own list must be kept");
        // The existing Claude's side is untouched.
        assert!(source.join("local_a.json").exists());
        assert!(!source.join("local_b.json").exists());

        // Idempotent.
        assert!(!link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert_eq!(fs::read_link(&own).unwrap(), source);
    }

    #[test]
    fn link_does_nothing_unless_shared() {
        let f = fixture(SharingConfig::share_settings_preset());
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");

        assert!(!link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));
        assert!(own.join("local_b.json").exists());
    }

    #[test]
    fn link_does_nothing_without_exactly_one_folder_on_each_side() {
        // Not signed in yet: no account folder in the environment.
        let f = shared();
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        assert!(!link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert!(account_folders(&env_desktop(&f)).is_empty());

        // Nothing to share yet on the existing Claude side.
        let f = shared();
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        assert!(!link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));

        // Two accounts on the existing Claude side: ambiguous.
        let f = shared();
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        account_dir(&f.default_desktop, ACCT_B, ORG_A, "local_c.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        assert!(!link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));

        // Two accounts in the environment: ambiguous.
        let f = shared();
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        account_dir(&env_desktop(&f), ACCT_A, ORG_B, "local_d.json");
        assert!(!link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));
    }

    #[test]
    fn link_repoints_a_link_to_the_wrong_place() {
        let f = shared();
        let source = account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let elsewhere = f._tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let own = env_desktop(&f).join(INDEX_DIR).join(ACCT_B).join(ORG_B);
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &own).unwrap();

        assert!(link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert_eq!(fs::read_link(&own).unwrap(), source);
        assert!(elsewhere.exists(), "the old link target is not touched");
    }

    #[test]
    fn link_restores_the_environment_list_when_the_symlink_fails() {
        let tmp_f = shared();
        let provider = MockPlatformProvider::new(
            tmp_f.default_desktop.clone(),
            tmp_f._tmp.path().join("cli_default"),
            tmp_f._tmp.path().join("app"),
        )
        .with_symlink_failure(true);
        account_dir(&tmp_f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&tmp_f), ACCT_B, ORG_B, "local_b.json");

        assert!(link(&tmp_f.profile, &tmp_f.default_desktop, &provider).is_err());
        assert!(!provider.is_symlink(&own));
        assert!(
            own.join("local_b.json").exists(),
            "own list must be restored"
        );
        let kept = own.with_file_name(format!("{ORG_B}{LOCAL_SUFFIX}"));
        assert!(!kept.exists());
    }

    #[test]
    fn link_leaves_an_existing_kept_list_alone() {
        let f = shared();
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        let kept = own.with_file_name(format!("{ORG_B}{LOCAL_SUFFIX}"));
        fs::create_dir_all(&kept).unwrap();
        fs::write(kept.join("local_old.json"), "{}").unwrap();

        assert!(!link(&f.profile, &f.default_desktop, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));
        assert!(own.join("local_b.json").exists());
        assert!(kept.join("local_old.json").exists());
    }

    #[test]
    fn link_refuses_to_operate_inside_the_default_dir() {
        // A misconfigured environment whose Desktop dir IS the default dir.
        let mut f = shared();
        f.profile.isolation.desktop_user_data_dir = f.default_desktop.clone();
        let source = account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let other = tempdir().unwrap();
        account_dir(other.path(), ACCT_B, ORG_B, "local_b.json");

        assert!(link(&f.profile, other.path(), &f.provider).is_err());
        assert!(!f.provider.is_symlink(&source));
        assert!(source.join("local_a.json").exists());
    }

    #[test]
    fn inspect_reports_each_state() {
        // Shared, but the environment has no account folder yet.
        let f = shared();
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        assert_eq!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::PendingLaunch {
                account_folder_exists: false
            }
        );

        // Signed in, link to be made on the next launch.
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        assert_eq!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::PendingLaunch {
                account_folder_exists: true
            }
        );
        assert!(
            !ItemHealth::PendingLaunch {
                account_folder_exists: true
            }
            .is_issue()
        );

        // Linked.
        link(&f.profile, &f.default_desktop, &f.provider).unwrap();
        assert!(matches!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::SharedOk { .. }
        ));

        // Linked somewhere else: fixable.
        fs::remove_file(&own).unwrap();
        std::os::unix::fs::symlink(f._tmp.path(), &own).unwrap();
        assert!(matches!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::WrongTarget { fixable: true, .. }
        ));

        // A second account in the environment: ambiguous, an issue.
        account_dir(&env_desktop(&f), ACCT_A, ORG_B, "local_d.json");
        assert_eq!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::AccountAmbiguous
        );
        assert!(ItemHealth::AccountAmbiguous.is_issue());

        // Nothing on the existing Claude side.
        let f = shared();
        account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        assert_eq!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::SourceAbsent
        );
    }

    #[test]
    fn inspect_isolated_list() {
        let f = fixture(SharingConfig::share_settings_preset());
        let source = account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        assert_eq!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::IsolatedOk
        );
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        assert_eq!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::IsolatedOk
        );

        // A link where the list must stay the environment's own.
        fs::remove_dir_all(&own).unwrap();
        std::os::unix::fs::symlink(&source, &own).unwrap();
        assert!(matches!(
            inspect(&f.profile, &f.default_desktop, &f.provider),
            ItemHealth::UnexpectedLink { .. }
        ));
    }

    #[test]
    fn prepare_launch_links_unless_the_environment_runs() {
        let f = shared();
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");

        let running = MockPlatformProvider::new(
            f.default_desktop.clone(),
            f._tmp.path().join("cli_default"),
            f._tmp.path().join("app"),
        )
        .with_running_args(vec![format!(
            "/Applications/Claude.app/Contents/MacOS/Claude --user-data-dir={}",
            env_desktop(&f).display()
        )]);
        assert!(!prepare_launch(&f.profile, &running).unwrap());
        assert!(!f.provider.is_symlink(&own));

        assert!(prepare_launch(&f.profile, &f.provider).unwrap());
        assert!(f.provider.is_symlink(&own));
    }

    #[test]
    fn prepare_launch_ignores_the_existing_claude_and_other_sources() {
        let mut f = shared();
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        f.profile.sharing.source.profile = "another".to_string();
        assert!(!prepare_launch(&f.profile, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));

        let mut f = shared();
        f.profile.profile.is_default = true;
        account_dir(&f.default_desktop, ACCT_A, ORG_A, "local_a.json");
        let own = account_dir(&env_desktop(&f), ACCT_B, ORG_B, "local_b.json");
        assert!(!prepare_launch(&f.profile, &f.provider).unwrap());
        assert!(!f.provider.is_symlink(&own));
    }
}
