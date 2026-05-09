use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn schema_compatibility_cases() {
    for case in cases() {
        let output = captain([
            "check",
            "--before",
            &format!("tests/cases/{}/old/**/*.capnp", case),
            "--after",
            &format!("tests/cases/{}/new/**/*.capnp", case),
        ]);

        insta::assert_snapshot!(case, render_output(&output));
    }
}

#[test]
fn git_refs_compare_same_path() {
    let repo = temp_dir("captain-git-ref");
    fs::create_dir_all(repo.join("schemas")).unwrap();
    git(&repo, ["init", "-q"]);
    git(&repo, ["config", "user.email", "captain@example.com"]);
    git(&repo, ["config", "user.name", "Captain Tests"]);

    fs::write(
        repo.join("schemas/user.capnp"),
        concat!(
            "@0xbf5147cbbecf40c1;\n",
            "\n",
            "struct User {\n",
            "  id @0 :UInt64;\n",
            "  email @1 :Text;\n",
            "}\n",
        ),
    )
    .unwrap();
    git(&repo, ["add", "schemas/user.capnp"]);
    git(&repo, ["commit", "-q", "-m", "old schema"]);

    fs::write(
        repo.join("schemas/user.capnp"),
        concat!(
            "@0xbf5147cbbecf40c1;\n",
            "\n",
            "struct User {\n",
            "  id @0 :UInt64;\n",
            "  email @1 :Data;\n",
            "}\n",
        ),
    )
    .unwrap();
    git(&repo, ["add", "schemas/user.capnp"]);
    git(&repo, ["commit", "-q", "-m", "new schema"]);

    let output = Command::new(env!("CARGO_BIN_EXE_captain"))
        .current_dir(&repo)
        .args([
            "check",
            "--before-ref",
            "HEAD~1",
            "--after-ref",
            "HEAD",
            "--path",
            "schemas/**/*.capnp",
        ])
        .output()
        .unwrap();

    insta::assert_snapshot!("git-ref-same-path", render_output(&output));

    fs::remove_dir_all(repo).unwrap();
}

#[test]
fn compare_ref_uses_current_worktree() {
    let repo = temp_dir("captain-compare-ref");
    fs::create_dir_all(repo.join("schemas")).unwrap();
    git(&repo, ["init", "-q"]);
    git(&repo, ["config", "user.email", "captain@example.com"]);
    git(&repo, ["config", "user.name", "Captain Tests"]);

    fs::write(
        repo.join("schemas/user.capnp"),
        concat!(
            "@0xbf5147cbbecf40c1;\n",
            "\n",
            "struct User {\n",
            "  id @0 :UInt64;\n",
            "  email @1 :Text;\n",
            "}\n",
        ),
    )
    .unwrap();
    git(&repo, ["add", "schemas/user.capnp"]);
    git(&repo, ["commit", "-q", "-m", "baseline schema"]);

    fs::write(
        repo.join("schemas/user.capnp"),
        concat!(
            "@0xbf5147cbbecf40c1;\n",
            "\n",
            "struct User {\n",
            "  id @0 :UInt64;\n",
            "  email @1 :Data;\n",
            "}\n",
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_captain"))
        .current_dir(&repo)
        .args([
            "check",
            "--compare-ref",
            "HEAD",
            "--path",
            "schemas/**/*.capnp",
        ])
        .output()
        .unwrap();

    insta::assert_snapshot!("compare-ref-worktree", render_output(&output));

    fs::remove_dir_all(repo).unwrap();
}

fn cases() -> Vec<String> {
    let mut cases = std::fs::read_dir("tests/cases")
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    cases.sort();
    cases
}

fn captain<'a>(args: impl IntoIterator<Item = &'a str>) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_captain"))
        .args(args)
        .output()
        .unwrap()
}

fn git<'a>(repo: &Path, args: impl IntoIterator<Item = &'a str>) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("{}-{}-{nanos}", name, std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn render_output(output: &std::process::Output) -> String {
    format!(
        "status: {}\nstdout:\n{}stderr:\n{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}
