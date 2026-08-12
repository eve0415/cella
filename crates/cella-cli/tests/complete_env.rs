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
