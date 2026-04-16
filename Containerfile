FROM alpine:3.23.3
RUN apk add \
	bash \
	gcc \
	musl-dev \
	rustup \
	zig \
	libnftnl-dev \
	libmnl-dev
RUN rustup-init \
	--profile minimal \
	--target riscv64a23-unknown-linux-gnu \
	-y
COPY scripts/zig-cc-rva23.sh /scripts/zig-cc-rva23.sh
ENV CARGO_BUILD_TARGET="riscv64a23-unknown-linux-gnu"
ENV PATH="/root/.cargo/bin:$PATH"
ENV CC_riscv64a23_unknown_linux_gnu="/scripts/zig-cc-rva23.sh"
ENV CARGO_TARGET_RISCV64A23_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C linker=/scripts/zig-cc-rva23.sh"
