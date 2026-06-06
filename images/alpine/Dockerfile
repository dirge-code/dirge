# dirge-microvm — minimal Alpine guest image for the dirge microVM sandbox.
#
# Built by `dirge sandbox setup` via buildah.

FROM alpine:3.21.3

# /var/empty must exist so the tmpfs mount point is present at boot.
RUN mkdir -p /var/empty && chmod 755 /var/empty \
    && apk add --no-cache openssh-server \
    && ssh-keygen -A \
    && adduser -D sandbox \
    && passwd -u sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && echo 'PubkeyAuthentication yes' >> /etc/ssh/sshd_config \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh

HEALTHCHECK --interval=10s --timeout=3s --retries=3 \
    CMD pgrep sshd || exit 1

EXPOSE 22

CMD ["/usr/sbin/sshd", "-D", "-e"]
