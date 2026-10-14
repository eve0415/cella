//! Variable substitution for `devcontainer.metadata` labels read back from an
//! image or container.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use cella_backend::names::{CONFIG_FILE_LABEL, LOCAL_FOLDER_LABEL};
use cella_features::{FeatureContainerConfig, ImageMetadataUserInfo};

use crate::devcontainer::subst::SubstitutionContext;

/// Substitute devcontainer variables in every entry of a `devcontainer.metadata`
/// label, mirroring the official CLI, which substitutes each raw label entry
/// before merging it.
///
/// Each entry's `remoteEnv` is left raw: it is resolved once the container's
/// environment has been probed, so `${containerEnv:VAR}` sees the live
/// container. A label that is not a JSON array or object is returned unchanged.
#[must_use]
pub fn substitute_metadata_label(label: &str, ctx: &SubstitutionContext) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(label) else {
        return label.to_string();
    };
    match &mut value {
        serde_json::Value::Array(entries) => {
            for entry in entries {
                substitute_entry(entry, ctx);
            }
        }
        serde_json::Value::Object(_) => substitute_entry(&mut value, ctx),
        _ => return label.to_string(),
    }
    serde_json::to_string(&value).unwrap_or_else(|_| label.to_string())
}

/// Substitute one metadata entry, leaving its `remoteEnv` raw.
fn substitute_entry(entry: &mut serde_json::Value, ctx: &SubstitutionContext) {
    let Some(obj) = entry.as_object_mut() else {
        ctx.substitute_value(entry);
        return;
    };
    for (key, value) in obj.iter_mut() {
        if key != "remoteEnv" {
            ctx.substitute_value(value);
        }
    }
}

/// Parse a `devcontainer.metadata` label after substituting its entries with
/// [`substitute_metadata_label`].
///
/// For readers whose result is not substituted again downstream.
#[must_use]
pub fn parse_substituted_metadata(
    label: &str,
    ctx: &SubstitutionContext,
) -> (FeatureContainerConfig, ImageMetadataUserInfo) {
    cella_features::parse_image_metadata(&substitute_metadata_label(label, ctx))
}

/// Build the [`SubstitutionContext`] for a container when no devcontainer
/// config has been resolved, from the identity labels the container carries.
///
/// The local workspace is the `devcontainer.local_folder` label (else
/// `dev.cella.workspace_path`). `${devcontainerId}` hashes `id_labels` when
/// given, else the container's `devcontainer.local_folder` /
/// `devcontainer.config_file` labels. Without a workspace label only
/// `${localEnv:...}` expands.
#[must_use]
pub fn container_subst_ctx<S: std::hash::BuildHasher>(
    labels: &HashMap<String, String, S>,
    id_labels: Option<&BTreeMap<String, String>>,
    container_workspace_folder: Option<&str>,
    env: impl IntoIterator<Item = (String, String)>,
) -> SubstitutionContext {
    let Some(local_folder) = labels
        .get(LOCAL_FOLDER_LABEL)
        .or_else(|| labels.get("dev.cella.workspace_path"))
    else {
        return SubstitutionContext::local_env_only(env.into_iter().collect());
    };
    let devcontainer_id = id_labels.map_or_else(
        || {
            let identity: BTreeMap<String, String> = [LOCAL_FOLDER_LABEL, CONFIG_FILE_LABEL]
                .into_iter()
                .filter_map(|key| labels.get(key).map(|v| (key.to_string(), v.clone())))
                .collect();
            crate::devcontainer::resolve::spec_devcontainer_id(&identity)
        },
        crate::devcontainer::resolve::spec_devcontainer_id,
    );
    SubstitutionContext::new(
        Path::new(local_folder),
        container_workspace_folder,
        &devcontainer_id,
        env.into_iter().collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn ctx() -> SubstitutionContext {
        let env = HashMap::from([("X".to_string(), "hello".to_string())]);
        SubstitutionContext::new(
            Path::new("/projects/app"),
            Some("/workspaces/app"),
            "the-id",
            env,
        )
    }

    fn parse(label: &str) -> Value {
        serde_json::from_str(label).unwrap()
    }

    #[test]
    fn substitutes_each_entry() {
        let label = json!([
            {"id": "feature", "postCreateCommand": concat!("echo ${localEnv", ":X}")},
            {"containerEnv": {"ID": concat!("${devcontainerId", "}")}},
        ])
        .to_string();
        let out = parse(&substitute_metadata_label(&label, &ctx()));
        assert_eq!(out[0]["postCreateCommand"], "echo hello");
        assert_eq!(out[1]["containerEnv"]["ID"], "the-id");
    }

    #[test]
    fn leaves_remote_env_raw() {
        let raw_path = concat!("${containerEnv", ":PATH}:/x");
        let label = json!([{"remoteEnv": {"PATH": raw_path}}]).to_string();
        let out = parse(&substitute_metadata_label(&label, &ctx()));
        assert_eq!(out[0]["remoteEnv"]["PATH"], raw_path);
    }

    #[test]
    fn single_object_label_is_substituted() {
        let label = json!({"postStartCommand": concat!("${localEnv", ":X}")}).to_string();
        let out = parse(&substitute_metadata_label(&label, &ctx()));
        assert_eq!(out["postStartCommand"], "hello");
    }

    #[test]
    fn invalid_label_is_returned_unchanged() {
        assert_eq!(substitute_metadata_label("not json", &ctx()), "not json");
    }

    #[test]
    fn parse_substituted_metadata_substitutes_users_and_collections() {
        let label = json!([{
            "remoteUser": concat!("${localEnv", ":X}"),
            "containerEnv": {"ID": concat!("${devcontainerId", "}")},
            "mounts": [concat!("source=${localEnv", ":X},target=/x,type=volume")],
        }])
        .to_string();
        let (config, user) = parse_substituted_metadata(&label, &ctx());
        assert_eq!(user.remote_user.as_deref(), Some("hello"));
        assert_eq!(
            config.container_env.get("ID").map(String::as_str),
            Some("the-id")
        );
        assert_eq!(config.mounts, vec!["source=hello,target=/x,type=volume"]);
    }

    /// Substitution is a single pass: an env value that itself looks like a
    /// variable stays literal.
    #[test]
    fn parse_substituted_metadata_does_not_rescan_values() {
        let literal = concat!("${localWorkspaceFolder", "}");
        let env = HashMap::from([("X".to_string(), literal.to_string())]);
        let ctx = SubstitutionContext::new(Path::new("/projects/app"), None, "id", env);
        let label =
            json!([{"mounts": [concat!("source=${localEnv", ":X},target=/x")]}]).to_string();
        let (config, _) = parse_substituted_metadata(&label, &ctx);
        assert_eq!(config.mounts, vec![format!("source={literal},target=/x")]);
    }

    #[test]
    fn container_ctx_uses_identity_labels() {
        let labels = HashMap::from([
            (LOCAL_FOLDER_LABEL.to_string(), "/ws".to_string()),
            (
                CONFIG_FILE_LABEL.to_string(),
                "/ws/.devcontainer/devcontainer.json".to_string(),
            ),
        ]);
        let ctx = container_subst_ctx(&labels, None, Some("/workspaces/ws"), HashMap::new());
        assert_eq!(
            ctx.substitute_str(concat!("${devcontainerId", "}")),
            crate::devcontainer::resolve::devcontainer_id(
                Path::new("/ws"),
                Path::new("/ws/.devcontainer/devcontainer.json"),
            )
        );
        assert_eq!(
            ctx.substitute_str(concat!("${localWorkspaceFolder", "}")),
            "/ws"
        );
        assert_eq!(
            ctx.substitute_str(concat!("${containerWorkspaceFolder", "}")),
            "/workspaces/ws"
        );
    }

    #[test]
    fn container_ctx_prefers_id_labels_for_devcontainer_id() {
        let labels = HashMap::from([(LOCAL_FOLDER_LABEL.to_string(), "/ws".to_string())]);
        let id_labels = BTreeMap::from([("a".to_string(), "1".to_string())]);
        let ctx = container_subst_ctx(&labels, Some(&id_labels), None, HashMap::new());
        assert_eq!(
            ctx.substitute_str(concat!("${devcontainerId", "}")),
            crate::devcontainer::resolve::spec_devcontainer_id(&id_labels)
        );
    }

    #[test]
    fn container_ctx_without_workspace_label_expands_local_env_only() {
        let env = HashMap::from([("X".to_string(), "hello".to_string())]);
        let ctx = container_subst_ctx(&HashMap::new(), None, None, env);
        assert_eq!(ctx.substitute_str(concat!("${localEnv", ":X}")), "hello");
        let literal = concat!("${localWorkspaceFolder", "}");
        assert_eq!(ctx.substitute_str(literal), literal);
    }
}
