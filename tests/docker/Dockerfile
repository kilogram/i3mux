# One image for both test roles:
#   start-sway.sh  -> headless Sway client (service i3mux-test-sway)
#   sshd -D -e     -> remote host         (service i3mux-remote-ssh)
FROM ubuntu:24.04

# Pinned shpool release. Update both values together.
ARG SHPOOL_VERSION=0.11.5
ARG SHPOOL_SHA256=132f786bd275749a46224c17a071474a850f0be1fbb752b1c96d910778543c9b

ENV DEBIAN_FRONTEND=noninteractive
ENV XDG_RUNTIME_DIR=/tmp/runtime-root
ENV WLR_BACKENDS=headless
ENV WLR_LIBINPUT_NO_DEVICES=1
ENV WLR_RENDERER=pixman
ENV LIBGL_ALWAYS_SOFTWARE=1

RUN apt-get update && apt-get install -y --no-install-recommends \
    # Sway/Wayland
    sway \
    foot \
    grim \
    wlr-randr \
    # SSH client and server
    openssh-client \
    openssh-server \
    # Network fault injection (remote)
    iproute2 \
    iptables \
    iputils-ping \
    net-tools \
    sudo \
    # shpool download, tree inspection in tests, tput in color-fill.sh
    ca-certificates \
    curl \
    jq \
    ncurses-bin \
    && rm -rf /var/lib/apt/lists/*

# shpool: prebuilt static binary. The tarball holds one file, ./shpool.
RUN curl -fsSL -o /tmp/shpool.tar.gz \
      "https://github.com/shell-pool/shpool/releases/download/v${SHPOOL_VERSION}/shpool-x86_64-unknown-linux-musl.tar.gz" \
    && echo "${SHPOOL_SHA256}  /tmp/shpool.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/shpool.tar.gz -C /usr/local/bin shpool \
    && chmod 755 /usr/local/bin/shpool \
    && rm /tmp/shpool.tar.gz \
    && shpool version

# Test user for the ssh remote. HOME is writable: i3mux keeps state in
# ~/.local/{run,state}/i3mux and ~/.config/i3mux there.
RUN useradd -m -s /bin/bash testuser && \
    echo 'testuser:testpass' | chpasswd && \
    usermod -aG sudo testuser && \
    echo 'testuser ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers && \
    touch /home/testuser/.sudo_as_admin_successful

# SSH server
RUN mkdir -p /var/run/sshd && \
    mkdir -p /home/testuser/.ssh && \
    chmod 700 /home/testuser/.ssh && \
    chown testuser:testuser /home/testuser/.ssh && \
    sed -i 's/#PermitRootLogin prohibit-password/PermitRootLogin yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PasswordAuthentication yes/PasswordAuthentication yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PubkeyAuthentication yes/PubkeyAuthentication yes/' /etc/ssh/sshd_config

RUN mkdir -p /opt/i3mux-test \
    && mkdir -p /root/.ssh/sockets \
    && mkdir -p /tmp/screenshots \
    && mkdir -p $XDG_RUNTIME_DIR \
    && chmod 700 /root/.ssh \
    && chmod 700 /root/.ssh/sockets \
    && chmod 700 $XDG_RUNTIME_DIR

COPY sway-test-config /opt/i3mux-test/sway-test-config
COPY start-sway.sh /opt/i3mux-test/start-sway.sh
COPY scripts/msg /opt/i3mux-test/bin/msg
COPY scripts/launch_terminal /opt/i3mux-test/bin/launch_terminal
RUN chmod 755 /opt/i3mux-test/start-sway.sh /opt/i3mux-test/bin/*
ENV PATH="/opt/i3mux-test/bin:$PATH"

# color-fill.sh: fill the terminal with one color and sleep until killed
RUN mkdir -p /opt/i3mux-test/color-scripts && \
    cat > /opt/i3mux-test/color-scripts/color-fill.sh << 'SCRIPT'
#!/bin/bash
# color-fill.sh - Fill terminal with solid color and sleep until killed
export TERM=${TERM:-xterm}
COLOR=${1:-41}
clear 2>/dev/null; tput civis 2>/dev/null
trap 'tput cnorm 2>/dev/null; clear 2>/dev/null; exit 0' INT TERM EXIT HUP
fill() {
    local ROWS=$(tput lines 2>/dev/null || echo 24)
    local COLS=$(tput cols 2>/dev/null || echo 80)
    clear 2>/dev/null
    echo -ne "\033[${COLOR}m"
    for ((i=0;i<ROWS;i++)); do printf "%${COLS}s\n" " "; done
}
trap fill WINCH; fill
while true; do sleep 1; done
SCRIPT
RUN chmod +x /opt/i3mux-test/color-scripts/color-fill.sh

# ssh client config for the remote service (ControlMaster for speed)
RUN printf '%s\n' \
      'Host i3mux-remote-ssh' \
      '  HostName i3mux-remote-ssh' \
      '  User testuser' \
      '  Port 22' \
      '  StrictHostKeyChecking no' \
      '  UserKnownHostsFile /dev/null' \
      '  ControlMaster auto' \
      '  ControlPath /root/.ssh/sockets/%r@%h:%p' \
      '  ControlPersist 600' \
      > /root/.ssh/config \
    && chmod 600 /root/.ssh/config

WORKDIR /opt/i3mux-test

EXPOSE 22
