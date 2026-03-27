PREFIX := "/usr/local"
BINDIR := PREFIX / "bin"
LIBEXECDIR := PREFIX / "libexec"
DATADIR := PREFIX / "share"

build: build_agent

build_agent:
    cargo build --release -p odorobo-agent

install: install_script install_unit install_agent

install_script:
    install -Dm755 systemd/scripts/odorobo-preflight {{ LIBEXECDIR }}/odorobo-preflight
    install -Dm755 systemd/scripts/odorobo-postflight {{ LIBEXECDIR }}/odorobo-postflight
    install -Dm755 systemd/scripts/odorobo-cleanup {{ LIBEXECDIR }}/odorobo-cleanup

install_unit:
    install -Dm644 systemd/odorobo-ch@.service {{ DATADIR }}/systemd/user/odorobo-ch@.service

install_agent:
    install -Dm755 target/release/odorobo-agent {{ BINDIR }}/odorobo-agent
