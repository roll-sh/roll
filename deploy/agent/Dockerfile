# The roll code agents sandbox: a machine roll hosts for an owner (docs/code/agents.md,
# "Sandboxes"). It runs `roll agent up` as the non-root user `roll` with the machine token roll
# gives it in its environment (ROLL_API_URL, ROLL_AGENT_TOKEN, ROLL_AGENT_MACHINE, …), and carries
# git, node, ripgrep and the Codex and Claude Code CLIs at pinned versions. Providers are logged in
# by the owner, in a terminal on the canvas (`claude /login`, `codex login --device-auth`), or
# get API keys through the sandbox's env.
#
# Build from the repository root:
#   docker build -f deploy/agent/Dockerfile -t roll-agent-sandbox .
# On agent37, register it as the workspace template ROLL_AGENT37_TEMPLATE names (default
# `roll-agent-sandbox`): `npx agent37 templates build` from a context holding this build's
# output, or a registry `image_ref`. It serves no HTTP, so it has no `default_port`. The instance's
# disk is mounted on /home/roll (the image user's home), so nothing the image needs lives there.
FROM rust:1-trixie AS build
# `dev` builds faster for tests; images you deploy use `release`.
ARG PROFILE=release
ARG CARGO_BUILD_JOBS=8
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=roll-agent-target,sharing=locked,target=/src/target \
    --mount=type=cache,id=roll-cargo-registry,sharing=locked,target=/usr/local/cargo/registry \
    CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} \
    cargo build --profile "$PROFILE" -p roll-cli --bin roll \
 && dir=$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE") \
 && cp "target/$dir/roll" /usr/local/bin/roll \
 && strip --strip-debug /usr/local/bin/roll

FROM node:24-trixie-slim
# The provider CLIs, pinned: a new version is a new image (and a template revision on agent37).
ARG CODEX_VERSION=0.164.0
ARG CLAUDE_CODE_VERSION=2.1.310
# tini reaps what agents leave behind and forwards SIGTERM so `roll agent up` stops cleanly.
# The rest is what coding agents reach for from their shell.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates tini git openssh-client ripgrep curl less procps jq unzip xz-utils make python3 locales \
 && sed -i 's/^# *en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen && locale-gen \
 && rm -rf /var/lib/apt/lists/* \
 && npm install -g --omit=dev "@openai/codex@${CODEX_VERSION}" "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && npm cache clean --force \
 && git config --system init.defaultBranch main \
 && useradd --create-home --uid 10001 --shell /bin/bash roll
COPY --from=build /usr/local/bin/roll /usr/local/bin/roll
USER roll
WORKDIR /home/roll
# IS_SANDBOX: Claude Code allows full access in a sandbox it's told about (it refuses as root
# otherwise; this runs as `roll` anyway). The CLIs are pinned in the image: they don't update
# themselves into the home volume.
ENV IS_SANDBOX=1 \
    SHELL=/bin/bash \
    LANG=en_US.UTF-8 \
    TERM=xterm-256color \
    DISABLE_AUTOUPDATER=1
ENTRYPOINT ["/usr/bin/tini", "-s", "--"]
CMD ["roll", "agent", "up"]
