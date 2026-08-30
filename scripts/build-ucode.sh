#!/bin/sh
# obserwrt - build the pinned ucode revision used by CI.
#
# Builds exactly the ucode commit that OpenWrt 25.12 ships (package/utils/ucode
# Makefile, PKG_SOURCE_VERSION below) with its upstream modules, so tests run
# against the same bytecode/API the target router uses. Bump the SHA here
# (and only here) when moving to a newer OpenWrt.
#
# Usage: sh scripts/build-ucode.sh <output-dir>
# Prints two paths on stdout:
#   line 1: the ucode binary
#   line 2: the directory containing the built module .so files
set -eu

# The ucode revision OpenWrt 25.12 ships (package/utils/ucode Makefile on the
# openwrt-25.12 branch, PKG_SOURCE_VERSION). Bump here to match the target.
UCODE_SHA="85922056ef7abeace3cca3ab28bc1ac2d88e31b1"

out="${1:?usage: build-ucode.sh <output-dir>}"
rm -rf "$out"
mkdir -p "$out"

git init -q "$out/src"
git -C "$out/src" remote add origin https://github.com/jow-/ucode
git -C "$out/src" fetch --depth 1 origin "$UCODE_SHA"
git -C "$out/src" checkout -q FETCH_HEAD

# The older 25.12 ucode source trips -Werror on current gcc (discarded-qualifiers)
# in its own code; we build it only as a test binary, so neutralize -Werror via
# a compiler wrapper that appends -Wno-error last (the project sets -Werror itself).
cat > "$out/cc" <<'SH'
#!/bin/sh
exec cc "$@" -Wno-error
SH
chmod +x "$out/cc"

cmake -S "$out/src" -B "$out/src/build" -DBUILD_UCODE_MODULES=ON \
	-DCMAKE_C_COMPILER="$out/cc" >"$out/configure.log" 2>&1
cmake --build "$out/src/build" -j"$(nproc)" >"$out/build.log" 2>&1

printf '%s\n' "$out/src/build/ucode" "$out/src/build"
