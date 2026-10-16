use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::error::{CswError, Result};
use crate::platform::PlatformProvider;

pub struct MacOsProvider {
    home_dir: PathBuf,
}

impl Default for MacOsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MacOsProvider {
    pub fn new() -> Self {
        Self {
            home_dir: dirs::home_dir().expect("Failed to determine home directory"),
        }
    }
}

impl PlatformProvider for MacOsProvider {
    fn claude_desktop_default_dir(&self) -> PathBuf {
        self.home_dir.join("Library/Application Support/Claude")
    }

    fn claude_cli_default_dir(&self) -> PathBuf {
        self.home_dir.join(".claude")
    }

    fn claude_desktop_app_path(&self) -> PathBuf {
        PathBuf::from("/Applications/Claude.app")
    }

    fn app_data_dir(&self) -> PathBuf {
        self.home_dir.join(".context-switcher-claude")
    }

    fn create_symlink(&self, target_path: &Path, link_path: &Path) -> Result<()> {
        // Guard: target must exist
        if !target_path.exists() {
            return Err(CswError::Other(format!(
                "Symlink target does not exist: {}",
                target_path.display()
            )));
        }

        // Guard: link_path must not already exist (non-destructive)
        if link_path.exists() && !link_path.is_symlink() {
            return Err(CswError::NonDestructiveViolation(
                link_path.display().to_string(),
            ));
        }

        // Remove existing symlink if present
        if link_path.is_symlink() {
            std::fs::remove_file(link_path)?;
        }

        // Create parent directories if needed
        if let Some(parent) = link_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::os::unix::fs::symlink(target_path, link_path).map_err(|e| CswError::SymlinkFailed {
            source: link_path.display().to_string(),
            target: target_path.display().to_string(),
            cause: e,
        })
    }

    fn remove_symlink(&self, link_path: &Path) -> Result<()> {
        if !link_path.is_symlink() {
            return Err(CswError::Other(format!(
                "Not a symlink: {}",
                link_path.display()
            )));
        }
        std::fs::remove_file(link_path)?;
        Ok(())
    }

    fn is_symlink(&self, path: &Path) -> bool {
        path.is_symlink()
    }

    fn launch_claude_desktop(
        &self,
        user_data_dir: &Path,
        cli_config_dir: Option<&Path>,
    ) -> Result<()> {
        let app_path = self.claude_desktop_app_path();
        if !app_path.exists() {
            return Err(CswError::DesktopNotInstalled);
        }

        let mut cmd = Command::new("open");
        cmd.arg("-n").arg("-a").arg(&app_path);

        if let Some(cli_dir) = cli_config_dir {
            cmd.arg("--env")
                .arg(format!("CLAUDE_CONFIG_DIR={}", cli_dir.display()));
        }

        cmd.arg("--args")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .spawn()
            .map_err(|e| CswError::Other(format!("Failed to launch Claude Desktop: {e}")))?;

        Ok(())
    }

    fn is_claude_desktop_running(&self) -> Result<bool> {
        Ok(!self.claude_desktop_pids()?.is_empty())
    }

    fn claude_desktop_pids(&self) -> Result<Vec<u32>> {
        // `-a` keeps pgrep's own ancestors in the match list. Run in a terminal
        // inside Claude, csw descends from that Claude, which must still count
        // as running (`csw switch` refuses while it runs).
        let output = Command::new("pgrep")
            .arg("-a")
            .arg("-f")
            .arg("Claude.app/Contents/MacOS/Claude")
            .output()?;

        desktop_pids_from_output(&output)
    }

    fn move_to_trash(&self, path: &std::path::Path) -> Result<()> {
        // Explicitly use the NSFileManager backend: the trash crate's default
        // on macOS drives Finder via AppleScript, which needs an Automation
        // permission prompt and can time out (observed on a real machine), and
        // would contradict docs/PRIVACY.md (CSW runs no osascript). The direct
        // API moves the folder into the user's Trash, restorable until the
        // Trash is emptied. Never falls back to permanent deletion on failure.
        use trash::TrashContext;
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut ctx = TrashContext::default();
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        ctx.delete(path)
            .map_err(|e| CswError::TrashMoveFailed(e.to_string()))
    }

    fn running_desktop_args(&self) -> Result<Vec<String>> {
        let pids = self.claude_desktop_pids()?;
        if pids.is_empty() {
            return Ok(vec![]);
        }
        let pid_list = pids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // `ps -o args=` prints one full command line per pid, no header.
        let output = Command::new("ps")
            // In the C locale, macOS ps escapes non-ASCII paths into M-/^
            // notation, which would hide running environments from matching.
            .env("LC_ALL", "en_US.UTF-8")
            .arg("-p")
            .arg(pid_list)
            .arg("-o")
            .arg("args=")
            .output()?;
        desktop_args_from_output(&output)
    }

    fn running_desktop_processes(&self) -> Result<Vec<(u32, String)>> {
        let pids = self.claude_desktop_pids()?;
        if pids.is_empty() {
            return Ok(vec![]);
        }
        let pid_list = pids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // `ps -o pid=,args=` prints "<pid> <full command line>" per pid, no header.
        let output = Command::new("ps")
            .arg("-p")
            .arg(pid_list)
            .arg("-o")
            .arg("pid=,args=")
            .output()?;
        if !output.status.success() {
            return Ok(vec![]);
        }
        Ok(main_process_pid_lines(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    fn activate_pid(&self, pid: u32) -> Result<bool> {
        use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
        // NSRunningApplication is thread-safe. This reads only the target's
        // activation state and asks the window server to bring its windows
        // forward; it never injects code into Claude nor reads window contents.
        // Called only from the explicit "前面に表示" action, never polled.
        match NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32) {
            Some(app) => {
                Ok(app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows))
            }
            None => Ok(false),
        }
    }

    fn frontmost_app(&self) -> Result<super::FrontmostApp> {
        // Only the application's PID is read; window titles and contents are
        // never touched. Called from an explicit user action (tray item).
        let pid = {
            let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
            match workspace.frontmostApplication() {
                Some(app) => app.processIdentifier(),
                None => return Ok(super::FrontmostApp::OtherApp),
            }
        };
        if pid == std::process::id() as i32 {
            return Ok(super::FrontmostApp::ThisApp);
        }
        // `ps -o args=` for just that process; a non-Claude frontmost app (or a
        // Claude helper process, which never owns the frontmost window) yields
        // no main-process line.
        let output = Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("args=")
            .output()?;
        if !output.status.success() {
            return Ok(super::FrontmostApp::OtherApp);
        }
        Ok(
            match main_process_arg_lines(&String::from_utf8_lossy(&output.stdout))
                .into_iter()
                .next()
            {
                Some(line) => super::FrontmostApp::Claude(line),
                None => super::FrontmostApp::OtherApp,
            },
        )
    }
}

// Keep command-result validation separate so failures can be tested without
// inspecting or stopping real applications. Only pgrep's documented no-match
// exit is evidence of no processes; ps failures require the caller to retry.
fn desktop_pids_from_output(output: &Output) -> Result<Vec<u32>> {
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(vec![]);
    }
    let stdout = process_inspection_stdout("pgrep", output)?;
    let pids: Vec<u32> = stdout
        .lines()
        .map(|line| {
            line.trim()
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or_else(|| {
                    CswError::Other(
                        "Could not inspect running Claude Desktop processes: invalid pgrep PID"
                            .into(),
                    )
                })
        })
        .collect::<Result<_>>()?;
    if pids.is_empty() {
        return Err(CswError::Other(
            "Could not inspect running Claude Desktop processes: pgrep returned no PIDs on success"
                .into(),
        ));
    }
    Ok(pids)
}

fn desktop_args_from_output(output: &Output) -> Result<Vec<String>> {
    let stdout = process_inspection_stdout("ps", output)?;
    if stdout.trim().is_empty() {
        return Err(CswError::Other(
            "Could not inspect running Claude Desktop processes: ps returned no arguments. Try again."
                .into(),
        ));
    }
    // macOS ps can exit successfully even when retrieving a process's
    // arguments fails, emitting only `(comm)` for that row. A partial list
    // must not be used to authorize deletion of a potentially running target.
    if stdout.lines().any(|line| {
        let line = line.trim();
        line.starts_with('(') && line.ends_with(')')
    }) {
        return Err(CswError::Other(
            "Could not inspect running Claude Desktop processes: ps could not read arguments. Try again."
                .into(),
        ));
    }
    Ok(main_process_arg_lines(stdout))
}

fn process_inspection_stdout<'a>(command: &str, output: &'a Output) -> Result<&'a str> {
    if !output.status.success() {
        return Err(CswError::Other(format!(
            "Could not inspect running Claude Desktop processes: {command} failed ({}). Try again.",
            output.status,
        )));
    }
    std::str::from_utf8(&output.stdout).map_err(|_| {
        CswError::Other(format!(
            "Could not inspect running Claude Desktop processes: {command} returned invalid UTF-8",
        ))
    })
}

/// Keep only the Claude Desktop *main* process lines from `ps -o args=` output.
/// Electron spawns helper processes (`--type=renderer`, `--type=gpu-process`, …)
/// that do not represent a running environment; only the main process carries the
/// meaningful `--user-data-dir`. Lines that are not the Claude binary are dropped.
fn main_process_arg_lines(ps_output: &str) -> Vec<String> {
    ps_output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| l.contains("Claude.app/Contents/MacOS/Claude"))
        .filter(|l| !l.contains("--type="))
        .map(str::to_string)
        .collect()
}

/// Parse `ps -o pid=,args=` output into `(pid, args line)` for each Claude main
/// process. Each line is "<pid> <full command line>"; the leading token is the
/// pid, the rest is the args line filtered exactly like [`main_process_arg_lines`]
/// (main process only, Electron helpers dropped).
fn main_process_pid_lines(ps_output: &str) -> Vec<(u32, String)> {
    ps_output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid_str, args) = line.split_once(char::is_whitespace)?;
            let pid: u32 = pid_str.trim().parse().ok()?;
            let args = args.trim();
            if args.contains("Claude.app/Contents/MacOS/Claude") && !args.contains("--type=") {
                Some((pid, args.to_string()))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_output(status: i32, stdout: &[u8]) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(status),
            stdout: stdout.to_vec(),
            stderr: vec![],
        }
    }

    #[test]
    fn process_inspection_distinguishes_no_matches_from_pgrep_failure() {
        assert!(
            desktop_pids_from_output(&process_output(1 << 8, b""))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            desktop_pids_from_output(&process_output(0, b"42\n84\n")).unwrap(),
            vec![42, 84]
        );
        for status in [2 << 8, 3 << 8, 9] {
            assert!(desktop_pids_from_output(&process_output(status, b"")).is_err());
        }
    }

    #[test]
    fn process_inspection_rejects_unreadable_pid_lists() {
        for stdout in [b"42\nbad-pid\n".as_slice(), b"\xff", b"0\n", b""] {
            assert!(desktop_pids_from_output(&process_output(0, stdout)).is_err());
        }
    }

    #[test]
    fn process_inspection_rejects_ps_failure_even_after_processes_exit() {
        // A pid can exit between pgrep and ps. Refuse this attempt; a retry
        // can establish that no matching process remains.
        for status in [1 << 8, 2 << 8, 9] {
            assert!(desktop_args_from_output(&process_output(status, b"")).is_err());
        }
    }

    #[test]
    fn process_inspection_rejects_unreadable_args() {
        for stdout in [
            b"\xff".as_slice(),
            b"",
            b" \n",
            b"(Claude)\n",
            b"/Applications/Claude.app/Contents/MacOS/Claude --user-data-dir=/test/other\n(Claude)\n",
        ] {
            assert!(desktop_args_from_output(&process_output(0, stdout)).is_err());
        }
    }

    #[test]
    fn process_inspection_keeps_successful_main_process_args() {
        let args = b"/Applications/Claude.app/Contents/MacOS/Claude --user-data-dir=/test/Environment Data\n";
        assert_eq!(
            desktop_args_from_output(&process_output(0, args)).unwrap(),
            vec![String::from_utf8(args.to_vec()).unwrap().trim().to_string()]
        );
    }

    #[test]
    fn main_process_pid_lines_pairs_pid_with_main_and_drops_helpers() {
        let ps = "  101 /Applications/Claude.app/Contents/MacOS/Claude --user-data-dir=/p/A/desktop-data\n\
                    102 /Applications/Claude.app/Contents/MacOS/Claude --type=renderer --user-data-dir=/p/A/desktop-data\n\
                    103 /Applications/Claude.app/Contents/MacOS/Claude --type=gpu-process\n\
                    104 /usr/bin/some-unrelated-process\n\
                  \n";
        let pairs = main_process_pid_lines(ps);
        assert_eq!(pairs.len(), 1, "only the main Claude process survives");
        assert_eq!(pairs[0].0, 101);
        assert!(pairs[0].1.contains("--user-data-dir=/p/A/desktop-data"));
        assert!(!pairs[0].1.contains("--type="));
    }

    #[test]
    fn main_process_arg_lines_keeps_main_drops_helpers_and_others() {
        let ps = "/Applications/Claude.app/Contents/MacOS/Claude --user-data-dir=/p/A/desktop-data\n\
                  /Applications/Claude.app/Contents/MacOS/Claude --type=renderer --user-data-dir=/p/A/desktop-data\n\
                  /Applications/Claude.app/Contents/MacOS/Claude --type=gpu-process\n\
                  /usr/bin/some-unrelated-process\n\
                  \n";
        let lines = main_process_arg_lines(ps);
        assert_eq!(lines.len(), 1, "only the main Claude process survives");
        assert!(lines[0].contains("--user-data-dir=/p/A/desktop-data"));
        assert!(!lines[0].contains("--type="));
    }

    #[test]
    fn test_default_paths() {
        let provider = MacOsProvider::new();
        let desktop_dir = provider.claude_desktop_default_dir();
        assert!(
            desktop_dir
                .to_string_lossy()
                .contains("Application Support/Claude")
        );

        let cli_dir = provider.claude_cli_default_dir();
        assert!(cli_dir.to_string_lossy().ends_with(".claude"));

        let app_data = provider.app_data_dir();
        assert!(
            app_data
                .to_string_lossy()
                .ends_with(".context-switcher-claude")
        );
    }

    #[test]
    fn test_symlink_operations() {
        let provider = MacOsProvider::new();
        let tmp = std::env::temp_dir().join("csw-test-symlink");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let target = tmp.join("target.txt");
        std::fs::write(&target, "hello").unwrap();

        let link = tmp.join("link.txt");
        provider.create_symlink(&target, &link).unwrap();

        assert!(provider.is_symlink(&link));
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "hello");

        provider.remove_symlink(&link).unwrap();
        assert!(!link.exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_non_destructive_guard() {
        let provider = MacOsProvider::new();
        let tmp = std::env::temp_dir().join("csw-test-guard");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let target = tmp.join("target.txt");
        std::fs::write(&target, "original").unwrap();

        // Existing real file at link location should fail
        let existing = tmp.join("existing.txt");
        std::fs::write(&existing, "do not overwrite").unwrap();

        let result = provider.create_symlink(&target, &existing);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CswError::NonDestructiveViolation(_)
        ));

        // Verify existing file was not modified
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "do not overwrite"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The parent may use the C locale, where macOS ps escapes non-ASCII
    /// arguments. Exercise the real inspection with only this test process,
    /// without creating a profile or starting Claude.
    #[test]
    fn running_desktop_args_preserve_unicode_paths_in_c_locale() {
        const PROBE: &str = "CSW_TEST_UNICODE_CLAUDE_PROBE";
        const TARGET: &str = "/nonexistent/csw-test/仕事/desktop-data";
        if std::env::var_os(PROBE).is_some() {
            let provider = MacOsProvider::new();
            let args = provider.running_desktop_args().unwrap();
            assert!(
                crate::switcher::desktop_dir_running(
                    Path::new(TARGET),
                    &args,
                    &provider.claude_desktop_default_dir(),
                ),
                "the running Unicode target must match even with LC_ALL=C",
            );
            return;
        }
        use std::os::unix::process::CommandExt;
        let output = Command::new(std::env::current_exe().unwrap())
            .arg0(format!(
                "/nonexistent/csw-test/Claude.app/Contents/MacOS/Claude --user-data-dir={TARGET}"
            ))
            .args([
                "--exact",
                "platform::macos::tests::running_desktop_args_preserve_unicode_paths_in_c_locale",
                "--test-threads=1",
            ])
            .env(PROBE, "1")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the Unicode process probe must succeed: {}",
            String::from_utf8_lossy(&output.stdout),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    /// A Claude that hosts the terminal running csw (the Claude CSW opened, or
    /// a Claude Code session in the desktop app) is an ancestor of csw, and
    /// still counts as running (SPECIFICATION.md「利用中」の判定). Otherwise
    /// `csw switch` would not refuse while that Claude runs. macOS `pgrep`
    /// leaves out its own ancestors unless given `-a`.
    ///
    /// The test runs itself again with argv[0] set to a Claude main-process
    /// path, so the re-run is the parent of the `pgrep` it starts. Only argv[0]
    /// changes; no file is created at that path.
    #[test]
    fn claude_desktop_pids_include_an_ancestor_claude() {
        const PROBE: &str = "CSW_TEST_ANCESTOR_CLAUDE_PROBE";
        if std::env::var_os(PROBE).is_some() {
            let pids = MacOsProvider::new().claude_desktop_pids().unwrap();
            let own = std::process::id();
            assert!(pids.contains(&own), "pids {pids:?} miss the ancestor {own}");
            return;
        }
        use std::os::unix::process::CommandExt;
        let output = Command::new(std::env::current_exe().unwrap())
            .arg0("/nonexistent/csw-test/Claude.app/Contents/MacOS/Claude")
            .args([
                "--exact",
                "platform::macos::tests::claude_desktop_pids_include_an_ancestor_claude",
                "--test-threads=1",
            ])
            .env(PROBE, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the re-run named like a Claude main process must find itself: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the re-run must run the probe: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
