FROM ghcr.io/ultramarine-linux/ultramarine:43

# Install dependencies
RUN dnf do -y --action=install \
    rustup \
    clang-devel \
    cmake \
    ninja \
    just \
    git \
    zig \
    cloud-hypervisor-nightly
    
RUN mkdir -p /run/odorobo/vms
RUN chmod -R 1777 /run/odorobo