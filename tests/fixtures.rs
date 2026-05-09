use std::process::Command;

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

fn render_output(output: &std::process::Output) -> String {
    format!(
        "status: {}\nstdout:\n{}stderr:\n{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}
