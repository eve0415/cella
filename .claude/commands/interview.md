---
name: interview
description: Interview user in-depth to create a detailed spec. Use when gathering requirements for new features, understanding project needs, or planning implementation details.
argument-hint: [instructions]
allowed-tools: AskUserQuestion, Write, Read, using-superpowers, superpowers:brainstorm, superpowers:write-plan
model: opus
---

Follow the user instructions and interview me in detail using the AskUserQuestion tool about literally anything: technical implementation, UI & UX, concerns, tradeoffs, etc. but make sure the questions are not obvious. be very in-depth and continue interviewing me continually until it's complete. then, write the spec to a file.
Before interviewing users, check the codebase before asking questions to avoid asking questions that can be found by looking at the codebase.

Interview me relentlessly about every aspect of this plan until we reach a shared understanding. Walk down each branch of the design tree, resolving dependencies between decisions one-by-one.

<instructions>$ARGUMENTS</instructions>

Use `using-superpowers` for the start of the session. And then, use `superpowers:brainstorm` and also `superpowers:write-plan` skills/commands/tools.
