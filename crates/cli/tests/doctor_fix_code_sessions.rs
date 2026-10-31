//! `csw doctor --fix` が、v0.24.8 が Codeタブの会話一覧に残したリンクを取り除き、
//! 退避した環境だけの一覧を元の名前に戻すことを確かめる (docs/SPECIFICATION.md §3
//! 「Codeタブの会話一覧」、§5.B の `csw doctor`)。
//!
//! core の `doctor_fix_links` が戻せても、CLI は「直せる項目がある」と判定した
//! ときにだけそれを呼ぶ。この判定にこの項目が無いと、`--fix` は何もせずに同じ
//! 問題を報告し続ける。

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

const ACCT: &str = "11111111-1111-4111-8111-111111111111";
const ORG: &str = "22222222-2222-4222-8222-222222222222";

fn csw(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_csw"))
        .args(args)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .expect("run csw")
}

/// 空の一時フォルダを HOME にして、本物の環境の一覧に触れないようにする。
fn temp_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "csw-doctor-fix-code-sessions-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("create a temporary HOME");
    home
}

#[test]
fn doctor_fix_removes_the_v0248_link_and_restores_the_kept_list() {
    let home = temp_home();
    let out = csw(
        &home,
        &["profile", "create", "research", "--mode", "share_workspace"],
    );
    assert!(
        out.status.success(),
        "create: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // v0.24.8 が残す状態: アカウントのフォルダの下の項目が既存のClaudeの一覧への
    // リンクで、環境だけの一覧は <org>.csw-local に退避されている。リンクの先は
    // この HOME の中の別のフォルダにして、本物の既存のClaudeには触れない。
    let source = home.join("existing-list");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("local_a.json"), "{}").unwrap();
    let account = home
        .join(".context-switcher-claude/profiles/research/desktop-data/claude-code-sessions")
        .join(ACCT);
    let own = account.join(ORG);
    let kept = account.join(format!("{ORG}.csw-local"));
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join("local_b.json"), "{}").unwrap();
    std::os::unix::fs::symlink(&source, &own).unwrap();

    // 直す前: 問題として報告し、終了コードは 1。
    let out = csw(&home, &["doctor", "research"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Code tab conversation list") && stdout.contains("is a link to"),
        "{stdout}"
    );

    // --fix: リンクを取り除き、退避した一覧を元の名前に戻し、直した旨を出す。
    let out = csw(&home, &["doctor", "research", "--fix"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("FIXED") && stdout.contains("restored this environment's own list"),
        "{stdout}"
    );
    assert!(
        std::fs::symlink_metadata(&own).unwrap().is_dir(),
        "the own list is a real folder again"
    );
    assert!(own.join("local_b.json").exists(), "the kept list is back");
    assert!(
        std::fs::symlink_metadata(&kept).is_err(),
        "the kept name is gone"
    );
    assert!(
        source.join("local_a.json").exists(),
        "the link target is untouched"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}");

    // 直した後: 問題なし。
    let out = csw(&home, &["doctor", "research"]);
    assert_eq!(out.status.code(), Some(0));

    let _ = std::fs::remove_dir_all(&home);
}
