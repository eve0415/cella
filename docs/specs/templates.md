# Templates

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

cella is a drop-in replacement for the devcontainer CLI. All template handling behavior MUST match the [devcontainer Templates specification](https://containers.dev/implementors/templates/).

## Summary

Dev container templates are pre-configured project starters distributed via OCI registries. Each template consists of a metadata file (`devcontainer-template.json`), a devcontainer configuration, and optional supporting files. During application, user-provided option values are substituted into all template files before they are written to the target project.

## Template Metadata Schema

Each template MUST contain a `devcontainer-template.json` metadata file. The `id` field MUST match the name of the containing subdirectory.

| Property | Type | Required | Description |
|---|---|---|---|
| `id` | `string` | yes | Template identifier. MUST match the containing directory name. |
| `version` | `string` | yes | Semantic version (`MAJOR.MINOR.PATCH`). Authoritative version source. |
| `name` | `string` | yes | Display name |
| `description` | `string` | no | Human-readable overview |
| `documentationURL` | `string` | no | Link to documentation |
| `licenseURL` | `string` | no | Link to license |
| `publisher` | `string` | no | Maintainer identifier |
| `keywords` | `string[]` | no | Search terms |
| `platforms` | `string[]` | no | Supported languages and frameworks |
| `options` | `object` | no | User-configurable parameters (see [Template Options](#template-options)) |
| `optionalPaths` | `string[]` | no | Files users MAY exclude. Paths MUST be relative to the template directory root. |

### Version Requirements

- The `version` field MUST follow semantic versioning (`MAJOR.MINOR.PATCH`).
- The `version` field in `devcontainer-template.json` is the authoritative source of truth for the template's version.

### Template Options

Each key in the `options` object is a unique option ID. Option IDs MUST be unique within a single `devcontainer-template.json`.

| Property | Type | Required | Description |
|---|---|---|---|
| `type` | `string` | yes | MUST be exactly `"boolean"` or `"string"` |
| `description` | `string` | no | User-facing explanation |
| `default` | `string \| boolean` | yes | Fallback value. When `enum` is present, the default MUST be a member of the enum. |
| `proposals` | `string[]` | no | Suggested values (free-form input allowed) |
| `enum` | `string[]` | no | Restricted value list (no custom values) |

The `proposals` and `enum` fields are mutually exclusive. A template option MUST NOT declare both.

## OCI Distribution

Templates are distributed as OCI artifacts using the reference format:

```
<registry>/<namespace>/<template>[:<semantic-version>]
```

### Reference Resolution

- An absent version tag MUST resolve to `latest`.
- The full reference format MUST be `oci-registry/namespace/template[:version]`.

### Packaging

- Each template MUST be packaged as a tarball named `devcontainer-template-{id}.tgz`.
- The tarball MUST contain exactly the files from the template subdirectory.
- Downloaded template artifacts MUST match the OCI digest to prevent supply-chain substitution.
- The OCI push operation requires a resolved registry URL, namespace, and template ID before execution.

### Collection Metadata

When templates are published as a collection:

- The `devcontainer-collection.json` MUST contain `sourceInformation` and a `templates` array.
- The collection MUST include an entry for every template subdirectory in the repository.
- The `templates` array length MUST equal the number of successfully packaged templates.
- The collection namespace MUST match the template namespace without the template name suffix.
- All version tags (major, minor, patch, latest) for a template MUST reference identical content.

## Template Application

### Option Resolution

All template options MUST be resolved before any file substitution begins. Resolution rules:

1. User-provided values override defaults.
2. Options not provided by the user MUST use the declared `default` value.
3. Unknown options (not declared in `devcontainer-template.json`) MUST be ignored.

### Option Substitution

Template files use the `${templateOption:optionId}` syntax for placeholder replacement. During application:

1. Every occurrence of `${templateOption:optionId}` MUST be replaced with the resolved option value.
2. Substitution MUST apply to ALL files within the template subdirectory, not just `devcontainer.json`.
3. Option values containing special characters MUST NOT cause placeholder injection -- values are treated as literal strings, not as substitution syntax.

### Template Output

After application, the template MUST produce one of:

- `.devcontainer.json` (root-level shorthand)
- `.devcontainer/devcontainer.json` (standard location)

The resulting configuration MUST be a valid devcontainer.json.
