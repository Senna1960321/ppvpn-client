#!/usr/bin/env bash
# Builds the CLI's release archive for one platform into dist/cli/:
#
#   ppvpn-cli-<version>-<platform>.tar.gz
#     ppvpn-cli-<version>-<platform>/ppvpn, README.md (crates/cli/README.md), LICENSE
#   ppvpn-cli-<version>-<platform>.symbols.tar.gz
#     ppvpn-cli-<version>-<platform>.symbols/ppvpn.debug (Linux) or ppvpn.dSYM (macOS)
#
# The shipped binary has no symbols; its symbols go into the second archive,
# which turns a backtrace's addresses into function names (docs/cli.md).
#
#   tools/cli/build-release.sh linux x86_64-unknown-linux-musl     # linux-x86_64
#   tools/cli/build-release.sh linux aarch64-unknown-linux-musl    # linux-aarch64
#   tools/cli/build-release.sh macos                               # macos-universal
#   tools/cli/build-release.sh macos aarch64-apple-darwin          # macos-arm64 (a merge, #459)
#
# Linux builds are static (musl, with mimalloc: crates/cli/Cargo.toml), made
# on an x86_64 Linux host as sail builds its musl releases: with sail's own
# scripts at the commit Cargo.lock pins (a pinned, hash-checked musl-cross
# GCC; BoringSSL as the prebuilt-boringssl action sets it up). The macOS
# build is one universal binary of aarch64 and x86_64, made on macOS; with
# one target named, that slice alone (never for stable).
#
# Environment:
#   PPVPN_CLI_STAGE
#                 macOS only: build compiles unsigned target slices and exits;
#                 package signs and archives those slices without compiling.
#                 The default, all, builds and packages in one invocation.
#   CHANNEL       empty (a merge: the crate's version), dev or stable
#   VERSION       x.y.z of a dev or stable run. stable: must be the crate's
#                 version (Cargo.toml), or the build fails
#   PPVPN_API_BASE_DEFAULT
#                 the backend, compiled in (required; release.yml: the GitHub
#                 variable PPVPN_API_BASE, for dev the secret, #517). dev: a dev build
#                 (PPVPN_BUILD_PROFILE=dev, which also takes PPVPN_API_BASE at
#                 run time); a dev archive is never published
#   MACOS_SIGNING_P12_BASE64, MACOS_SIGNING_P12_PASSWORD
#                 macOS: our Developer ID Application certificate and key, as the
#                 desktop app is signed (#471); without them ad-hoc (refused for
#                 stable)
#   ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_P8
#                 macOS: App Store Connect API key, notarizes the binary (required
#                 for stable)
set -euo pipefail

os=${1:?usage: $0 linux <target> | macos [<target>...]}
stage_mode=${PPVPN_CLI_STAGE:-all}
case "$os:$stage_mode" in
linux:all | macos:all | macos:build | macos:package) ;;
*) echo "build-release: invalid PPVPN_CLI_STAGE=$stage_mode for $os" >&2; exit 1 ;;
esac
cd "$(dirname "$0")/../.."
root=$(pwd)
work=$(mktemp -d)

fail() {
	echo "build-release: $*" >&2
	exit 1
}

crate=$(cargo metadata --locked --no-deps --format-version 1 |
	jq -r '.packages[] | select(.name == "ppvpn-cli") | .version')
[ -n "$crate" ] || fail "no ppvpn-cli in cargo metadata"

# What the binary is built as: release builds take no profile or version
# from the environment, and only the backend given.
unset PPVPN_BUILD_PROFILE PPVPN_VERSION
[ -n "${PPVPN_API_BASE_DEFAULT:-}" ] || fail "PPVPN_API_BASE_DEFAULT (the backend) is empty"
export PPVPN_API_BASE_DEFAULT
case "${CHANNEL:-}" in
"")
	version=$crate
	name_version=$version
	;;
stable)
	[ "${VERSION:-}" = "$crate" ] || fail "the release's version ${VERSION:-(none)} is not the CLI's $crate (Cargo.toml)"
	version=$crate
	name_version=$version
	;;
dev)
	[ -n "${VERSION:-}" ] || fail "a dev build needs VERSION"
	version=$VERSION
	name_version=$version-dev
	export PPVPN_BUILD_PROFILE=dev PPVPN_VERSION=$version
	;;
*) fail "unknown channel $CHANNEL" ;;
esac

case $os in
linux)
	target=${2:?usage: $0 linux <target>}
	case $target in
	x86_64-unknown-linux-musl) platform=linux-x86_64 ;;
	aarch64-unknown-linux-musl) platform=linux-aarch64 ;;
	*) fail "unsupported target $target" ;;
	esac
	rev=$(awk '/^name = "sail"$/ {f=1; next} f && /^source = / {sub(/.*#/, ""); sub(/"$/, ""); print; exit}' Cargo.lock)
	[ ${#rev} -eq 40 ] || fail "no sail commit in Cargo.lock"
	sail=$(mktemp -d)
	git clone --quiet --filter=blob:none --no-checkout https://github.com/peakpassvpn/sail.git "$sail"
	git -C "$sail" checkout --quiet "$rev"
	(cd "$sail" && bash scripts/install_cross_toolchain.sh "$target" >/dev/null)
	bash "$sail/scripts/cross.sh" "$target" build --release --locked -p ppvpn-cli
	rm -rf "$sail"
	# The toolchain's binutils know the target's ELF.
	tools=$(echo "${SAIL_CROSS_DIR:-$HOME/.sail-cross}"/musl-*/"$target"/bin)
	[ -x "$tools/$target-objcopy" ] || fail "no $target-objcopy in the cross toolchain"
	symbols=$work/ppvpn.debug
	bin=$work/ppvpn
	"$tools/$target-objcopy" --only-keep-debug "$root/target/$target/release/ppvpn" "$symbols"
	"$tools/$target-objcopy" --strip-all --add-gnu-debuglink="$symbols" "$root/target/$target/release/ppvpn" "$bin"
	"$tools/$target-nm" "$symbols" | grep 'ppvpn_cli8commands3run' >/dev/null || fail "ppvpn.debug lacks the CLI's symbols"
	! "$tools/$target-nm" "$bin" 2>/dev/null | grep . >/dev/null || fail "the shipped ppvpn still has symbols"
	# Static: it runs on any Linux of its architecture, glibc or not.
	info=$(file -b "$bin")
	echo "$info"
	grep -Eq 'static(ally|-pie) linked' <<<"$info" || fail "ppvpn is not static: $info"
	case $target in
	x86_64-*)
		grep -q 'x86-64' <<<"$info" || fail "not an x86-64 binary: $info"
		reported=$("$bin" version)
		echo "$reported"
		[ "$reported" = "ppvpn $version" ] || fail "ppvpn reports '$reported', not $version"
		;;
	aarch64-*) grep -q 'ARM aarch64' <<<"$info" || fail "not an aarch64 binary: $info" ;;
	esac
	;;
macos)
	shift
	targets=("$@")
	[ ${#targets[@]} -gt 0 ] || targets=(aarch64-apple-darwin x86_64-apple-darwin)
	archs=()
	for target in "${targets[@]}"; do
		case $target in
		aarch64-apple-darwin) archs+=(arm64) ;;
		x86_64-apple-darwin) archs+=(x86_64) ;;
		*) fail "unsupported target $target" ;;
		esac
		if [ "$stage_mode" != package ]; then
			cargo build --release --locked -p ppvpn-cli --target "$target"
		fi
		[ -f "target/$target/release/ppvpn" ] || fail "missing prebuilt CLI slice: $target"
	done
	case "${archs[*]}" in
	"arm64 x86_64") platform=macos-universal ;;
	arm64) platform=macos-arm64 ;;
	*) fail "macos builds aarch64-apple-darwin, or it and x86_64-apple-darwin in that order" ;;
	esac
	[ "$platform" = macos-universal ] || [ "${CHANNEL:-}" != stable ] || fail "a stable build is universal"
	if [ "$stage_mode" = build ]; then
		# No certificate, keychain, notary or final archive is touched here.
		rmdir "$work"
		exit 0
	fi
	bin=$root/target/$platform/ppvpn
	mkdir -p "$(dirname "$bin")"
	slices=()
	for target in "${targets[@]}"; do slices+=("target/$target/release/ppvpn"); done
	lipo -create -output "$bin" "${slices[@]}"
	lipo "$bin" -verify_arch "${archs[@]}"
	# Symbols out before signing: a dSYM (the symbol tables of both slices;
	# release builds carry no DWARF, which dsymutil warns about), then strip.
	symbols=$work/ppvpn.dSYM
	dsymutil "$bin" -o "$symbols" 2>&1 | grep -v 'no debug symbols in executable' || true
	for arch in "${archs[@]}"; do
		nm -arch "$arch" "$symbols/Contents/Resources/DWARF/ppvpn" | grep 'ppvpn_cli8commands3run' >/dev/null \
			|| fail "the dSYM lacks the CLI's symbols ($arch)"
	done
	strip "$bin"
	! nm -arch arm64 "$bin" 2>/dev/null | grep 'ppvpn_cli8commands3run' >/dev/null || fail "the shipped ppvpn still has symbols"
	# The desktop app's Developer ID (tools/desktop/ci/build-macos-native.sh):
	# Apple's designated requirement (identifier, Team ID) carries across
	# updates and certificate renewals, and with it the Keychain item's
	# access. A temporary keychain holds it for this build only.
	identifier=com.peakpassvpn.ppvpn.cli
	team=$(tr -d '[:space:]' <crates/service/src/macos_team_id)
	identity=""
	if [ -n "${MACOS_SIGNING_P12_BASE64:-}" ]; then
		: "${MACOS_SIGNING_P12_PASSWORD:?MACOS_SIGNING_P12_PASSWORD is required with MACOS_SIGNING_P12_BASE64}"
		key_dir=$(mktemp -d)
		keychain=$key_dir/signing.keychain-db
		keychain_password=$(uuidgen)
		original_keychains=()
		while IFS= read -r line; do original_keychains+=("$(echo "$line" | tr -d ' "')"); done \
			< <(security list-keychains -d user)
		cleanup() {
			security list-keychains -d user -s ${original_keychains[@]+"${original_keychains[@]}"} || true
			security delete-keychain "$keychain" || true
			rm -rf "$key_dir"
		}
		trap cleanup EXIT
		security create-keychain -p "$keychain_password" "$keychain"
		security set-keychain-settings -lut 21600 "$keychain"
		security unlock-keychain -p "$keychain_password" "$keychain"
		printf '%s' "$MACOS_SIGNING_P12_BASE64" | base64 --decode >"$key_dir/signing.p12"
		# security import reads only the legacy PKCS#12 algorithms and reports
		# OpenSSL 3's defaults as "MAC verification failed ... (wrong password?)".
		security import "$key_dir/signing.p12" -k "$keychain" -P "$MACOS_SIGNING_P12_PASSWORD" -T /usr/bin/codesign >/dev/null ||
			fail "MACOS_SIGNING_P12_BASE64 could not be imported. If the password is right and the .p12 came from OpenSSL 3, re-export it with \`openssl pkcs12 -export -legacy\` or from Keychain Access."
		rm -f "$key_dir/signing.p12"
		security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$keychain_password" "$keychain" >/dev/null
		security list-keychains -d user -s "$keychain" ${original_keychains[@]+"${original_keychains[@]}"}
		identity=$(security find-identity -v "$keychain" |
			sed -n 's/^ *[0-9][0-9]*) [0-9A-F]\{40\} "\(.*\)"$/\1/p' |
			grep -x "Developer ID Application: .* ($team)" || true)
		if [ -z "$identity" ] || [ "$(printf '%s\n' "$identity" | wc -l)" -ne 1 ]; then
			security find-identity -v "$keychain" >&2
			[ "${CHANNEL:-}" != stable ] || fail "the certificate is not one \"Developer ID Application: … ($team)\" identity"
			# A dev build whose environment still holds another certificate
			# (the self-signed one of 0.7.x): built ad hoc, as without one.
			echo "::warning::MACOS_SIGNING_P12_BASE64 is not our Developer ID Application certificate; signing ad-hoc (not for release)"
			identity=""
		fi
	fi
	if [ -n "$identity" ]; then
		codesign --force --sign "$identity" --identifier "$identifier" --options runtime --timestamp "$bin"
		codesign -dv --verbose=4 "$bin" 2>&1 | grep -qx "TeamIdentifier=$team" || fail "ppvpn is not signed by $team"
		if [ -n "${ASC_KEY_P8:-}" ]; then
			: "${ASC_KEY_ID:?ASC_KEY_ID is required with ASC_KEY_P8}"
			: "${ASC_ISSUER_ID:?ASC_ISSUER_ID is required with ASC_KEY_P8}"
			(umask 077 && printf '%s\n' "$ASC_KEY_P8" >"$key_dir/notary.p8")
			# A bare binary takes no stapled ticket: Gatekeeper looks it up
			# online.
			ditto -c -k "$bin" "$work/ppvpn.zip"
			"$root/tools/notarize.sh" "$work/ppvpn.zip" "$key_dir/notary.p8" "$ASC_KEY_ID" "$ASC_ISSUER_ID" ||
				fail "notarizing ppvpn failed"
		else
			[ "${CHANNEL:-}" != stable ] || fail "a stable build must be notarized (ASC_KEY_P8)"
			echo "warning: ASC_KEY_P8 not set; signed but not notarized"
		fi
	else
		[ "${CHANNEL:-}" != stable ] || fail "a stable build must be signed (MACOS_SIGNING_P12_BASE64)"
		[ -n "${MACOS_SIGNING_P12_BASE64:-}" ] ||
			echo "warning: MACOS_SIGNING_P12_BASE64 not set; signing ad-hoc (not for release)"
		codesign --force --sign - --identifier "$identifier" --options runtime "$bin"
	fi
	codesign --verify --strict --verbose=2 "$bin"
	reported=$("$bin" version)
	echo "$reported"
	[ "$reported" = "ppvpn $version" ] || fail "ppvpn reports '$reported', not $version"
	;;
*) fail "unknown os $os" ;;
esac

# pack <directory under $stage>: dist/cli/<directory>.tar.gz
pack() {
	if [ "$os" = linux ]; then
		# GNU tar: sorted names, no owner, the commit's time.
		tar -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$(git log -1 --format=%ct)" \
			-cf - "$1" | gzip -n >"dist/cli/$1.tar.gz"
	else
		# bsdtar: no AppleDouble (._*) files.
		COPYFILE_DISABLE=1 tar -C "$stage" --uid 0 --gid 0 --uname root --gname root -cf - "$1" |
			gzip -n >"dist/cli/$1.tar.gz"
	fi
	tar -tzvf "dist/cli/$1.tar.gz" | head -20
}

name=ppvpn-cli-$name_version-$platform
stage=$(mktemp -d)
mkdir -p dist/cli "$stage/$name" "$stage/$name.symbols"
cp "$bin" "$stage/$name/ppvpn"
chmod 755 "$stage/$name/ppvpn"
cp crates/cli/README.md "$stage/$name/README.md"
cp LICENSE "$stage/$name/LICENSE"
cp -R "$symbols" "$stage/$name.symbols/"
pack "$name"
pack "$name.symbols"
rm -rf "$stage" "$work"
ls -l dist/cli
