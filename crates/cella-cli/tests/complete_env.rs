//! `CELLA_COMPLETE=<shell> cella` must print the same hook as
//! `cella completion <shell>`. Nothing in-process can cover the
//! `CompleteEnv` call in `main`, so this drives the real binary.

#[test]
fn complete_env_matches_the_completion_subcommand() {
    let bin = env!("CARGO_BIN_EXE_cella");

    for shell in clap_complete::env::Shells::builtins().names() {
        let via_env = std::process::Command::new(bin)
            .env("CELLA_COMPLETE", shell)
            .output()
            .expect("run cella");
        assert!(
            via_env.status.success(),
            "CELLA_COMPLETE={shell} cella failed"
        );
        let via_env = String::from_utf8(via_env.stdout).expect("UTF-8");
        assert!(
            !via_env.is_empty(),
            "CELLA_COMPLETE={shell} must print a hook"
        );

        let via_sub = std::process::Command::new(bin)
            .env_remove("CELLA_COMPLETE")
            .args(["completion", shell])
            .output()
            .expect("run cella");
        assert!(via_sub.status.success(), "cella completion {shell} failed");
        let via_sub = String::from_utf8(via_sub.stdout).expect("UTF-8");

        assert_eq!(
            via_env, via_sub,
            "the two entry points must agree for {shell}"
        );
    }
}

/// Drive a real completion request and assert candidates come back.
///
/// The suite this replaced asserted the *generated script* named `switch`. The
/// dynamic hook deliberately inlines nothing, so that assertion had to go — but
/// dropping it left nothing checking the one thing a user notices: that
/// pressing TAB produces anything at all. Every other test here would still
/// pass against a binary that answered every request with an empty list.
#[test]
fn a_completion_request_returns_real_candidates() {
    // The hook re-invokes cella as `CELLA_COMPLETE=<shell> cella -- <words...>`
    // with `_CLAP_COMPLETE_INDEX` naming the word under the cursor.
    let complete = |index: &str, words: &[&str]| -> Vec<String> {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_cella"))
            .env("CELLA_COMPLETE", "bash")
            .env("_CLAP_COMPLETE_INDEX", index)
            .arg("--")
            .args(words)
            .output()
            .expect("run cella");
        assert!(
            out.status.success(),
            "completion request failed for {words:?}"
        );
        String::from_utf8(out.stdout)
            .expect("UTF-8")
            .lines()
            .map(str::to_owned)
            .collect()
    };

    let top = complete("1", &["cella", ""]);
    for expected in ["up", "down", "switch", "image", "completion"] {
        assert!(
            top.iter().any(|c| c == expected),
            "`cella <TAB>` must offer `{expected}`, got {top:?}"
        );
    }

    assert_eq!(
        complete("1", &["cella", "im"]),
        ["image"],
        "a unique prefix must complete to exactly one subcommand"
    );

    let nested = complete("2", &["cella", "image", ""]);
    assert!(
        nested.iter().any(|c| c == "update"),
        "`cella image <TAB>` must offer `update`, got {nested:?}"
    );
}

/// The same byte-identity check, but with `cella` invoked as a bare name off
/// `PATH` — which is how every real installation runs it.
///
/// `completer_path()` reproduces a `clap_complete` internal, and that
/// duplication is only licensed by a test that compares the two ends. The
/// comparison above cannot cover the bare-name branch: `CARGO_BIN_EXE_cella` is
/// absolute, so `components().count() > 1` always holds and the cwd-join branch
/// is always the one taken. Copying the binary somewhere else does not help —
/// that path is still multi-component. Only going through a shell, so argv[0]
/// is literally `cella`, exercises the other half.
#[test]
fn a_bare_name_invocation_emits_the_same_hook() {
    let dir = tempfile::tempdir().expect("tempdir");
    let on_path = dir.path().join("cella");
    std::fs::copy(env!("CARGO_BIN_EXE_cella"), &on_path).expect("copy cella");

    let run = |script: &str| -> String {
        let out = std::process::Command::new("sh")
            .args(["-c", script])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env_remove("CELLA_COMPLETE")
            // A predictable cwd, so a cwd-join regression shows up as a diff
            // rather than as an unstable string.
            .current_dir(dir.path())
            .output()
            .expect("run sh");
        assert!(
            out.status.success(),
            "`{script}` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("UTF-8")
    };

    for shell in clap_complete::env::Shells::builtins().names() {
        let via_sub = run(&format!("cella completion {shell}"));
        let via_env = run(&format!("CELLA_COMPLETE={shell} cella"));
        assert_eq!(
            via_env, via_sub,
            "bare-name entry points must agree for {shell}"
        );
        assert!(
            via_sub.contains("\"cella\"")
                || via_sub.contains("'cella'")
                || via_sub.contains(" cella "),
            "the hook must invoke the bare name, not an absolute path:\n{via_sub}"
        );
        assert!(
            !via_sub.contains(&dir.path().display().to_string()),
            "a bare-name invocation must not bake an absolute path into the hook:\n{via_sub}"
        );
    }
}
