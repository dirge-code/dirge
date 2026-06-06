# Dirge microVM guest image (Debian).
#
# Built by `dirge sandbox setup` via buildah.

FROM docker.io/library/debian:bookworm-20250224-slim

RUN mkdir -p /var/empty && chmod 755 /var/empty \
    && apt-get update \
    && apt-get install -y --no-install-recommends openssh-server ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && ssh-keygen -A \
    && adduser --system --no-create-home sshd \
    && adduser --disabled-password --gecos '' sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && echo '* - nofile 1048576' >> /etc/security/limits.conf \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh \
    && chmod 700 /home/sandbox

HEALTHCHECK --interval=10s --timeout=3s --retries=3 \
    CMD pgrep sshd || exit 1

EXPOSE 22

CMD ["/usr/sbin/sshd", "-D", "-e"]
