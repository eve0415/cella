# syntax=docker/dockerfile:1

# For config options, see the README at: https://github.com/devcontainers/templates/tree/main/src/rust
FROM mcr.microsoft.com/devcontainers/rust:2.0.8-trixie

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
  --mount=type=cache,target=/var/lib/apt,sharing=locked \
  du -sh /var/cache/apt && \
  rm /etc/apt/apt.conf.d/docker-clean && \
  apt-get update && \
  apt-get install -y xdg-utils tmux

USER vscode:vscode

RUN curl -fsSL https://claude.ai/install.sh | bash
