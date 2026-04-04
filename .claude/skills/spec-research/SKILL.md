---
name: spec-research
description: Research devcontainer spec, official CLI, and VS Code extension behavior before making design decisions. Use this skill whenever implementing devcontainer commands, options, lifecycle hooks, or features — or when behavior is ambiguous, when fixing spec compliance issues, or when the user asks how something works in the original devcontainer tools. Also use before answering questions about how cella should behave when you're not certain of the correct behavior. Trigger on any devcontainer property name, lifecycle hook, feature resolution, Docker Compose integration, port forwarding, or environment variable handling question.
---

Systematically investigate the original devcontainer ecosystem before implementing or making design decisions. The goal: understand exactly what the official tools do, find bugs users have reported, and determine how cella should behave — matching the spec where it's right, and fixing where it's broken.

Use `gh` CLI for all GitHub API calls. Never use WebFetch for GitHub URLs.

## Research Sequence

Work through these sources in order. Each builds on what the previous one found.

### 1. Spec Definition

Fetch the relevant spec page from containers.dev:

```sh
# Main property reference
gh api -H "Accept: application/vnd.github.v3.raw" /repos/devcontainers/spec/contents/docs/specs/devcontainer-reference.md

# Lifecycle hooks spec
gh api -H "Accept: application/vnd.github.v3.raw" /repos/devcontainers/spec/contents/docs/specs/devcontainerjson-reference.md

# Features spec (for OCI feature research)
gh api -H "Accept: application/vnd.github.v3.raw" /repos/devcontainers/spec/contents/docs/specs/devcontainer-features.md
```

Identify: required vs optional properties, default values, type constraints, and behavioral requirements.

### 2. Official CLI Implementation

Search `devcontainers/cli` for how the TypeScript CLI actually implements the behavior:

```sh
gh search code "<property-or-function>" --repo devcontainers/cli --limit 15
```

Then read the relevant source files to trace the exact logic. Pay attention to:
- Default values that differ from the spec
- Edge cases in parsing or validation
- Undocumented behaviors the CLI does that the spec doesn't mention
- Order of operations in multi-step processes

### 3. VS Code Extension Behavior

The VS Code extension often implements behavior differently from the CLI:

```sh
gh search issues "<topic> devcontainer" --repo microsoft/vscode-remote-release --limit 10 --sort updated
```

Note behavioral differences between CLI and extension — these are common and cella needs to decide which to follow.

### 4. Issue Trackers

Find bugs and limitations that frustrate users — this is where cella's value comes from:

```sh
# CLI bugs (sort by reactions = user impact)
gh search issues "<topic>" --repo devcontainers/cli --limit 10 --sort reactions

# Spec ambiguities and proposals
gh search issues "<topic>" --repo devcontainers/spec --limit 10 --sort updated

# Feature-specific issues (for OCI feature research)
gh search issues "<topic>" --repo devcontainers/features --limit 10

# Template-specific issues
gh search issues "<topic>" --repo devcontainers/templates --limit 5
```

### 5. Cross-Reference cella Codebase

Check what cella already implements for this topic:

```sh
# Search across all crates
rg "<property-or-keyword>" crates/ --type rust -l
```

Read the relevant files to understand current state and avoid duplicating work.

## Output: Research Summary

### Behavior Matrix

| Aspect | Spec says | CLI does | VS Code does | cella status |
|--------|-----------|----------|--------------|--------------|
| ... | ... | ... | ... | ... |

Highlight any row where columns disagree — these need a decision.

### Known Issues

List issues from trackers, sorted by user impact (reaction count):
- **Issue link** — user pain point, whether CLI/VS Code fixed it

### Spec Gaps

Anything the spec doesn't address but implementations handle or mishandle.

### Recommendation

- Match spec where it's clear and correct
- Match VS Code extension where spec is ambiguous (larger user base = stronger expectations)
- Fix CLI/extension bugs that users reported (link the issues as evidence)
- Document any intentional divergence with rationale

## Decision Priority

When sources conflict:
1. Explicit spec requirement (mandatory)
2. VS Code extension behavior (largest user base, strongest user expectations)
3. Official CLI behavior (reference implementation)
4. User issue consensus (what users actually want, measured by reactions)

Exception: if a spec requirement causes problems users consistently report, recommend fixing it and document why cella diverges.
