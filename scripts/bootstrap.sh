#!/bin/sh
# Download a release bundle; installation is handled by that release's installer.
set -eu

main() {
    system=$(uname -s)
    machine=$(uname -m)
    case "$system/$machine" in
        Darwin/arm64)
            package=harold-aarch64-apple-darwin
            tools='curl python3 shasum tar'
            checksum_command='shasum -a 256 -c'
            major=$(sw_vers -productVersion | cut -d . -f 1)
            if [ "$major" -lt 15 ]; then
                echo 'Harold requires macOS 15 or newer.' >&2
                return 1
            fi
            ;;
        Linux/x86_64)
            package=harold-x86_64-unknown-linux-gnu
            tools='curl python3 sha256sum tar'
            checksum_command='sha256sum -c'
            ;;
        *)
            echo "Harold release installation supports Apple Silicon Macs and x86_64 Linux. This machine is $system $machine." >&2
            return 1
            ;;
    esac
    for tool in $tools; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            echo "Required tool missing: $tool" >&2
            return 1
        fi
    done
    python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 9) else "Python 3.9+ is required")'
    temporary=$(mktemp -d)
    trap 'rm -rf "$temporary"' EXIT
    trap 'exit 1' HUP INT TERM
    base=https://github.com/kahgeh/harold/releases/latest/download
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --output "$temporary/$package.tar.gz.sha256" \
        "$base/$package.tar.gz.sha256"
    # Fail closed on checksum mismatch if latest changes between requests.
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --output "$temporary/$package.tar.gz" "$base/$package.tar.gz"
    (
        cd "$temporary"
        # Accept only the expected checksum filename before invoking the checksum command.
        python3 - "$package" <<'PY'
import pathlib, re, sys
name = sys.argv[1] + '.tar.gz'
value = pathlib.Path(name + '.sha256').read_text()
if not re.fullmatch(r'[0-9a-fA-F]{64}  ' + re.escape(name) + r'\n?', value):
    sys.exit('Invalid release checksum file')
PY
        $checksum_command "$package.tar.gz.sha256"
        # Reject archive paths/links that could escape the temporary directory.
        python3 - "$package" <<'PY'
import pathlib, sys, tarfile
root = sys.argv[1]
with tarfile.open(root + '.tar.gz') as archive:
    for item in archive.getmembers():
        path = pathlib.PurePosixPath(item.name)
        if (path.is_absolute() or '..' in path.parts or not path.parts
                or path.parts[0] != root or not (item.isfile() or item.isdir())):
            sys.exit('Unsafe release archive entry')
    archive.extractall('.')
PY
    )
    installer="$temporary/$package/scripts/install.py"
    # A curl pipe occupies stdin; reconnect the configuration wizard to the tty.
    if [ -t 0 ]; then
        python3 "$installer" --prebuilt-dir "$temporary/$package" "$@"
    elif ( : </dev/tty ) 2>/dev/null; then
        python3 "$installer" --prebuilt-dir "$temporary/$package" "$@" </dev/tty
    else
        python3 "$installer" --prebuilt-dir "$temporary/$package" "$@"
    fi
}

# Defining the entire program before running it keeps downloads off script stdin.
main "$@"
