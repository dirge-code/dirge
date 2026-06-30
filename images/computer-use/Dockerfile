# Dirge computer-use testing image.
#
# Xvfb + Firefox + xdotool + scrot + Python vision deps.
# Provides a virtual display so the computer_use plugin can be tested
# without touching the host desktop.
#
# Build:
#   buildah bud --storage-driver vfs --tag dirge-computer-use:latest -f images/computer-use/Dockerfile .
#
# Run:
#   podman run --rm -it dirge-computer-use:latest
#   # Inside: DISPLAY=:99 is already set, Xvfb is running on :99
#   # Run dirge and test computer:navigate etc.

FROM docker.io/library/debian:bookworm-20250224-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        # SSH (in case microvm testing is layered on top)
        openssh-server \
        # Virtual display
        xvfb \
        # Window manager (firefox needs one)
        openbox \
        # Browser
        firefox-esr \
        # Automation tools
        xdotool \
        # Screenshots
        scrot \
        # ImageMagick for display inspection
        imagemagick \
        # Python + vision deps
        python3 python3-pip \
        # OCR
        tesseract-ocr \
        # X11 utilities
        x11-utils \
    && rm -rf /var/lib/apt/lists/*

# Install Python vision dependencies
RUN pip3 install --break-system-packages \
        Pillow \
        numpy \
        pytesseract

# Set up sshd (for potential microvm use)
RUN ssh-keygen -A \
    && adduser --system --no-create-home sshd \
    && adduser --disabled-password --gecos '' sandbox \
    && echo 'PermitRootLogin no' >> /etc/ssh/sshd_config \
    && echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config \
    && echo '* - nofile 1048576' >> /etc/security/limits.conf \
    && mkdir -p /home/sandbox/.ssh && chmod 700 /home/sandbox/.ssh \
    && chmod 700 /home/sandbox \
    && mkdir -p /var/empty && chmod 755 /var/empty \
    && mkdir -p /workspace

# Entrypoint: start Xvfb and window manager, then hand over.
COPY images/computer-use/entrypoint.sh /usr/local/bin/entrypoint.sh
# libkrun init config: start Xvfb + openbox + sshd
COPY images/computer-use/krun_config.json /.krun_config.json
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV DISPLAY=:99

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/bin/bash"]
