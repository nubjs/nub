FROM buildkite/hosted-agent-base:latest@sha256:db770041c55b13a92ddb8365dc601a0141add0459dfd1d804f3e28926d4770da

ENV DEBIAN_FRONTEND=noninteractive
ENV MISE_EXPERIMENTAL=true
ENV MISE_YES=true

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    bash \
    build-essential \
    ca-certificates \
    curl \
    git \
    libssl-dev \
    parallel \
    pkg-config \
    xz-utils \
  && rm -rf /var/lib/apt/lists/*

# Download the installer before running it so network failures cannot yield an
# empty script and silently produce a mise-less image.
RUN set -eux; \
  curl --proto '=https' --tlsv1.2 -fsSL https://mise.run -o /tmp/mise-install.sh; \
  sh /tmp/mise-install.sh; \
  rm /tmp/mise-install.sh; \
  /root/.local/bin/mise --version
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable \
  && /root/.cargo/bin/rustup toolchain install 1.93.0 --profile minimal \
  && /root/.cargo/bin/rustup component add rustfmt clippy --toolchain stable \
  && rm -rf /root/.rustup/downloads /root/.rustup/tmp

ENV PATH="/root/.cargo/bin:/root/.local/bin:/root/.local/share/mise/shims:${PATH}"
