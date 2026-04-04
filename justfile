PREFIX := "/usr/local"
BINDIR := PREFIX / "bin"
LIBEXECDIR := PREFIX / "libexec"
DATADIR := PREFIX / "share"
USRLIBDIR := PREFIX / "lib"

# systemd unit load paths
# defaults to /etc, but can be overridden by setting SYSTEMD_UNITDIR to a different path
# usually /usr/lib/systemd/system

SYSTEMD_UNITDIR := "/etc"

build: build_agent

build_agent:
    cargo build --release -p odorobo-agent

build_agent_debug:
    cargo build -p odorobo-agent

debug: build_agent_debug
    sudo target/debug/odorobo-agent

install: install_script install_unit install_agent

install_script:
    install -Dm755 systemd/scripts/odorobo-preflight {{ LIBEXECDIR }}/odorobo-preflight
    install -Dm755 systemd/scripts/odorobo-postflight {{ LIBEXECDIR }}/odorobo-postflight
    install -Dm755 systemd/scripts/odorobo-cleanup {{ LIBEXECDIR }}/odorobo-cleanup

install_unit:
    install -Dm644 systemd/odorobo-ch@.service {{ SYSTEMD_UNITDIR }}/systemd/system/odorobo-ch@.service
    install -Dm644 systemd/odorobo-agent.service {{ SYSTEMD_UNITDIR }}/systemd/system/odorobo-agent.service
    systemctl daemon-reload || true

install_agent: build_agent
    install -Dm755 target/release/odorobo-agent {{ BINDIR }}/odorobo-agent

install_agent_debug:
    install -Dm755 target/debug/odorobo-agent {{ BINDIR }}/odorobo-agent
