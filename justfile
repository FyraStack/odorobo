PREFIX := "/usr/local"
BINDIR := PREFIX / "bin"
LIBEXECDIR := PREFIX / "libexec"
DATADIR := PREFIX / "share"
USRLIBDIR := PREFIX / "lib"

# systemd unit load paths
# defaults to /usr/lib, to get /usr/lib/sytemd/system
# can be overridden by setting SYSTEMD_UNITDIR to a different path

SYSTEMD_UNITDIR := "/usr/lib"

build: build_agent build_cli

build_agent:
    cargo build --release -p odorobo

build_cli:
    cargo build --release -p odoroboctl

build_debug:
    cargo build -p odorobo

debug: build_debug
    sudo target/debug/odorobo

install: install_unit install_agent install_ctl

install_unit:
    install -Dm644 systemd/odorobo.service -t {{ SYSTEMD_UNITDIR }}/systemd/system/
    systemctl daemon-reload || true

install_agent:
    install -Dm755 target/release/odorobo {{ BINDIR }}/odorobo

install_ctl:
    install -Dm755 target/release/odoroboctl {{ BINDIR }}/odoroboctl

install_debug:
    install -Dm755 target/debug/odorobo {{ BINDIR }}/odorobo
