use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PullPolicy {
    Always,
    #[default]
    Missing,
    Never,
    Build,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cli {
    #[serde(default)]
    pub verbose: bool,

    #[serde(default)]
    pub output: OutputFormat,

    #[serde(default)]
    pub pull: PullPolicy,

    #[serde(default)]
    pub skip_checksum: bool,

    #[serde(default)]
    pub no_network_rules: bool,

    #[serde(default)]
    pub build: CliBuild,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliBuild {
    #[serde(default)]
    pub no_cache: bool,

    #[serde(default)]
    pub profiles: Vec<String>,

    #[serde(default)]
    pub env_files: Vec<String>,

    #[serde(default)]
    pub pull_policy: PullPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let cli = Cli::default();
        assert!(!cli.verbose);
        assert_eq!(cli.output, OutputFormat::Text);
        assert_eq!(cli.pull, PullPolicy::Missing);
        assert!(!cli.skip_checksum);
        assert!(!cli.no_network_rules);
        assert!(!cli.build.no_cache);
        assert!(cli.build.profiles.is_empty());
        assert!(cli.build.env_files.is_empty());
        assert_eq!(cli.build.pull_policy, PullPolicy::Missing);
    }

    #[test]
    fn deserialize_output_format() {
        let cases = [
            (r#""text""#, OutputFormat::Text),
            (r#""json""#, OutputFormat::Json),
        ];
        for (json, expected) in cases {
            let fmt: OutputFormat = serde_json::from_str(json).unwrap();
            assert_eq!(fmt, expected);
        }
    }

    #[test]
    fn deserialize_pull_policy() {
        let cases = [
            (r#""always""#, PullPolicy::Always),
            (r#""missing""#, PullPolicy::Missing),
            (r#""never""#, PullPolicy::Never),
            (r#""build""#, PullPolicy::Build),
        ];
        for (json, expected) in cases {
            let pol: PullPolicy = serde_json::from_str(json).unwrap();
            assert_eq!(pol, expected);
        }
    }

    #[test]
    fn invalid_output_format_rejected() {
        let result = serde_json::from_str::<OutputFormat>(r#""yaml""#);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_pull_policy_rejected() {
        let result = serde_json::from_str::<PullPolicy>(r#""force""#);
        assert!(result.is_err());
    }

    #[test]
    fn empty_object_uses_defaults() {
        let cli: Cli = serde_json::from_str("{}").unwrap();
        assert!(!cli.verbose);
        assert_eq!(cli.output, OutputFormat::Text);
    }

    #[test]
    fn unknown_field_rejected() {
        let result = serde_json::from_str::<Cli>(r#"{"verbose":true,"extra":1}"#);
        assert!(result.is_err());
    }

    #[test]
    fn full_cli_roundtrip() {
        let json = r#"{
            "verbose": true,
            "output": "json",
            "pull": "always",
            "skip_checksum": true,
            "no_network_rules": true,
            "build": {
                "no_cache": true,
                "profiles": ["dev", "test"],
                "env_files": [".env.local"],
                "pull_policy": "never"
            }
        }"#;
        let cli: Cli = serde_json::from_str(json).unwrap();
        assert!(cli.verbose);
        assert_eq!(cli.output, OutputFormat::Json);
        assert_eq!(cli.pull, PullPolicy::Always);
        assert!(cli.skip_checksum);
        assert!(cli.no_network_rules);
        assert!(cli.build.no_cache);
        assert_eq!(cli.build.profiles, vec!["dev", "test"]);
        assert_eq!(cli.build.env_files, vec![".env.local"]);
        assert_eq!(cli.build.pull_policy, PullPolicy::Never);
    }
}
