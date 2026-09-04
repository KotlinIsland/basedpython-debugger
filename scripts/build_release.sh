#!/usr/bin/env bash
# build one platform's release of bpd, and prove it works before it is a file
# anybody could publish
#
# usage:
#
#   scripts/build_release.sh <version> <platform tag> <tag>=<interpreter>...
#
# for example, on a mac:
#
#   scripts/build_release.sh 0.0.1a1 macosx_11_0_arm64 \
#     3.13=/opt/python/3.13/bin/python3.13 3.14=…
#
# it runs on every platform a release is built for, which is why it is a script
# rather than steps in a workflow: the linux wheels are built inside a manylinux
# container and the others on the runner itself, and two copies of this would be
# two copies that drift
#
# the last three things it does are the ones worth having. a layout that
# assembles and verifies and cannot launch is the failure this whole crate was
# written against, so the wheel is installed with a real pip into a real venv
# and **every interpreter it carries an agent for is launched through it**
set -euo pipefail

if [ "$#" -lt 3 ]; then
    echo "usage: $0 <version> <platform tag> <tag>=<interpreter>..." >&2
    exit 2
fi

version=$1
platform=$2
shift 2

# what cargo leaves the agent called, and what the binary is called, both of
# which are the platform's rather than a choice. `bpd-release` joins its own
# answer for the agent's name inside the layout, so this is only about finding
# what was just built
case "$(uname -s)" in
    Darwin) agent=libbpd_agent.dylib exe= ;;
    Linux) agent=libbpd_agent.so exe= ;;
    MINGW* | MSYS* | CYGWIN*) agent=bpd_agent.dll exe=.exe ;;
    *)
        echo "$0: $(uname -s) is not a platform bpd is released for" >&2
        exit 1
        ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

staged=target/release-agents
layout=target/release-layout
dist=dist
rm -rf "$staged" "$layout" "$dist"

# one agent per interpreter. each build is against a different cpython, so each
# one has to be copied out before the next overwrites it — an agent that is
# still the previous interpreter's is one that imports and reads the wrong
# offsets, and nothing downstream can tell
carried=()
for pair in "$@"; do
    tag=${pair%%=*}
    python=${pair#*=}
    if [ "$tag" = "$pair" ] || [ -z "$python" ]; then
        echo "$0: \`$pair\` is not <tag>=<interpreter>" >&2
        exit 2
    fi

    # what the interpreter says it is, against what the tag claims. an agent
    # filed under a tag it was not built for is a release that refuses at
    # import on somebody else's machine, and this is the only moment both
    # halves are in one place
    said=$("$python" -c 'import sys, sysconfig; print("%d.%d%s" % (sys.version_info[0], sys.version_info[1], "t" if sysconfig.get_config_var("Py_GIL_DISABLED") else ""))')
    if [ "$said" != "$tag" ]; then
        echo "$0: \`$python\` says it is $said and it was given as $tag" >&2
        exit 1
    fi

    PYO3_PYTHON=$python cargo build --locked --release -p bpd_agent
    mkdir -p "$staged/$tag"
    cp "target/release/$agent" "$staged/$tag/$agent"
    carried+=(--agent "$tag=$staged/$tag/$agent")
done

cargo build --locked --release --bin bpd
cargo build --locked --release --bin bpd-release

release=target/release/bpd-release$exe
"$release" assemble --binary "target/release/bpd$exe" "${carried[@]}" --out "$layout"
"$release" verify "$layout"
"$release" wheel --layout "$layout" --version "$version" --platform "$platform" --out "$dist"

# and now the half no assertion about a zip can stand in for. the venv is built
# with the **oldest** interpreter carried, and every one of them is then debugged
# through the wheel installed into it — which is what a per-interpreter wheel
# could not do, and what the whole `py3-none-<platform>` decision rests on
first=$1
venv=target/release-venv
rm -rf "$venv"
"${first#*=}" -m venv "$venv"
if [ -d "$venv/Scripts" ]; then
    bin=$venv/Scripts
else
    bin=$venv/bin
fi
"$bin/python$exe" -m pip install --disable-pip-version-check "$dist"/*.whl

for pair in "$@"; do
    ran=$("$bin/bpd$exe" launch --python "${pair#*=}" -c 'print("the-program-ran")')
    if [ "$ran" != "the-program-ran" ]; then
        echo "$0: the installed wheel could not debug ${pair%%=*}: said \`$ran\`" >&2
        exit 1
    fi
    echo "$0: the installed wheel debugged ${pair%%=*}"
done

ls -l "$dist"
