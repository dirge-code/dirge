# Dirge microVM guest image — development variant with Rust.
#
# Built by `dirge sandbox setup --image dev` via buildah.

FROM docker.io/library/debian:bookworm-20250224-slim

RUN mkdir -p /var/empty && chmod 755 /var/empty \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        openssh-server \
        ca-certificates \
        curl \
        build-essential \
        pkg-config \
        libssl-dev \
        git \
        vim-tiny \
    && rm -rf /var/lib/apt/lists/*

# Install Rust via rustup (single toolchain, minimal).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable \
    --profile minimal \
    --component rustfmt,clippy \
    && /root/.cargo/bin/rustup component add rust-analyzer

# Move rustup/cargo to system paths so sandbox user can use them.
# RUSTUP_HOME must be set so rustup finds toolchains at their new path.
ENV RUSTUP_HOME="/usr/local/rustup"
ENV PATH="/usr/local/cargo/bin:${PATH}"
RUN mkdir -p /home/sandbox \
    && mv /root/.cargo /usr/local/cargo \
    && mv /root/.rustup /usr/local/rustup \
    && chown -R 1000:1000 /usr/local/cargo /usr/local/rustup /home/sandbox \
    && cargo --version \
    && rustc --version

RUN ssh-keygen -A \
    && adduser --system --no-create-home sshd \
    && adduser --disabled-password --gecos '' sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && echo 'AcceptEnv LANG LC_*' >> /etc/ssh/sshd_config \
    && echo '* - nofile 1048576' >> /etc/security/limits.conf \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh \
    && chmod 700 /home/sandbox

# Make Rust available to all users (sshd sources /etc/environment via PAM).
RUN echo 'PATH="/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"' >> /etc/environment \
    && echo 'RUSTUP_HOME="/usr/local/rustup"' >> /etc/environment

HEALTHCHECK --interval=10s --timeout=3s --retries=3 \
    CMD pgrep sshd || exit 1

EXPOSE 22

CMD ["/usr/sbin/sshd", "-D", "-e"]
