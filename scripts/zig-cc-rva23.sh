#!/usr/bin/env bash
set -euo pipefail

# A script for cargo to use as the C compiler and linker when building RVA23
# binaries. This links them using zig cc.

args=()
for arg in "$@"; do
	case "$arg" in
	--target=riscv64-unknown-linux-gnu)
		;;
	*)
		args+=("$arg")
		;;
	esac
done
exec zig cc -target riscv64-linux-gnu "${args[@]}"
