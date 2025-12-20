FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

# Install all required packages
RUN apt-get update && apt-get install -y \
    # X11/GUI (for xephyr)
    xvfb \
    i3 \
    xterm \
    scrot \
    imagemagick \
    x11-xserver-utils \
    dbus-x11 \
    # SSH client and server
    openssh-client \
    openssh-server \
    # Networking tools (for remote)
    iproute2 \
    iptables \
    iputils-ping \
    net-tools \
    sudo \
    # Build tools for abduco
    git \
    build-essential \
    libncurses-dev \
    jq \
    && rm -rf /var/lib/apt/lists/*

# Build and install abduco from source
RUN git clone https://github.com/martanne/abduco.git /tmp/abduco && \
    cd /tmp/abduco && \
    make && \
    make PREFIX=/usr install && \
    rm -rf /tmp/abduco

# Create test user (for SSH remote)
RUN useradd -m -s /bin/bash testuser && \
    echo 'testuser:testpass' | chpasswd && \
    usermod -aG sudo testuser && \
    echo 'testuser ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers

# Configure SSH server
RUN mkdir -p /var/run/sshd && \
    mkdir -p /home/testuser/.ssh && \
    chmod 700 /home/testuser/.ssh && \
    chown testuser:testuser /home/testuser/.ssh && \
    sed -i 's/#PermitRootLogin prohibit-password/PermitRootLogin yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PasswordAuthentication yes/PasswordAuthentication yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PubkeyAuthentication yes/PubkeyAuthentication yes/' /etc/ssh/sshd_config

# Create directories
RUN mkdir -p /opt/i3mux-test \
    && mkdir -p /root/.ssh/sockets \
    && mkdir -p /tmp/screenshots \
    && mkdir -p /tmp/i3mux/sessions /tmp/i3mux/locks \
    && chmod 700 /root/.ssh \
    && chmod 700 /root/.ssh/sockets \
    && chown -R testuser:testuser /tmp/i3mux

# Copy i3 test config
COPY i3-test-config /opt/i3mux-test/i3-test-config

# Copy startup script
COPY start-xephyr.sh /opt/i3mux-test/start-xephyr.sh
RUN chmod +x /opt/i3mux-test/start-xephyr.sh

# Create SSH config for ControlMaster
RUN echo "Host i3mux-remote-ssh" > /root/.ssh/config && \
    echo "  HostName i3mux-remote-ssh" >> /root/.ssh/config && \
    echo "  User testuser" >> /root/.ssh/config && \
    echo "  Port 22" >> /root/.ssh/config && \
    echo "  StrictHostKeyChecking no" >> /root/.ssh/config && \
    echo "  UserKnownHostsFile /dev/null" >> /root/.ssh/config && \
    echo "  ControlMaster auto" >> /root/.ssh/config && \
    echo "  ControlPath /root/.ssh/sockets/%r@%h:%p" >> /root/.ssh/config && \
    echo "  ControlPersist 600" >> /root/.ssh/config && \
    chmod 600 /root/.ssh/config

WORKDIR /opt/i3mux-test

EXPOSE 22
