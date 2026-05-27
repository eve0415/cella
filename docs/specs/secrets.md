# Secrets

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

cella is a drop-in replacement for the devcontainer CLI. All secrets handling behavior MUST match the devcontainer specification ([declarative secrets](https://github.com/devcontainers/spec/blob/main/docs/specs/declarative-secrets.md), [secrets support](https://github.com/devcontainers/spec/blob/main/docs/specs/secrets-support.md)).

## Summary

Dev containers support declarative secret declarations in `devcontainer.json`. Secrets are recommended to the user but never required -- container creation always succeeds without them. When secret values are provided through a secure mechanism, they are injected as environment variables before lifecycle hooks execute.

## Secret Declaration

The `secrets` property in `devcontainer.json` declares secrets the container expects. Keys MUST be valid Linux environment variable names per [POSIX](https://pubs.opengroup.org/onlinepubs/000095399/basedefs/xbd_chap08.html).

```jsonc
{
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
  "secrets": {
    "API_KEY": {
      "description": "API key for the external service.",
      "documentationUrl": "https://example.com/api-keys"
    },
    "DB_PASSWORD": {}
  }
}
```

### Secret Sub-Properties

All sub-properties are OPTIONAL. An empty object (`{}`) is a valid secret declaration.

| Property | Type | Required | Description |
|---|---|---|---|
| `description` | `string` | no | Brief description of the secret |
| `documentationUrl` | `string` | no | URL where the user can obtain or learn about the secret |

### Validation

- The declared secrets count MUST equal the processed secrets count in validation output.
- Invalid secret key names (not valid environment variable names) MUST produce warnings but MUST NOT block container creation.
- The parser SHOULD enforce an upper bound on the secrets map size to guard against resource exhaustion.

## Secret Injection

### Timing

Secret values MUST be injected before lifecycle hooks (`onCreateCommand`, `updateContentCommand`, `postCreateCommand`, `postStartCommand`, `postAttachCommand`) execute. This ensures lifecycle scripts can consume secrets as environment variables.

### Mechanism

Implementations MAY inject declared secrets into the container as environment variables, similar to `remoteEnv`. The injection mechanism SHOULD support dynamically changing secret values without requiring a container rebuild.

### Providing Secret Values

Secret values are NOT stored in `devcontainer.json`. A conforming implementation MUST provide a secure mechanism for users to supply secret values, such as:

- A secrets file (JSON format)
- Platform credential stores (macOS Keychain, Windows Credential Manager)
- External secret managers

## Graceful Degradation

Container creation MUST succeed when no secret values are provided. Secrets are recommendations, not requirements. A missing secret value MUST NOT:

- Prevent container creation
- Prevent container startup
- Block lifecycle hook execution

Load failures MUST return an error without leaving partial state or corrupted secrets in the container.

## Security

### Logging

Secret values MUST NOT appear in plaintext in:

- Container build logs
- Image layers
- CLI output
- Diagnostic dumps

### Storage

- The secrets source MUST NOT degrade to an insecure fallback when secure storage is unavailable. If secure storage is not available, the implementation SHOULD inform the user rather than silently falling back to plaintext storage.
- After injection, all provided secrets MUST be accessible as container environment variables through the standard environment variable interface.
