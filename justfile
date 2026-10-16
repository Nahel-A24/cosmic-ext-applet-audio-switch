name := 'cosmic-ext-applet-audio-switch'
appid := 'io.github.Nahel_A24.CosmicExtAppletAudioSwitch'

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')

bin-src := cargo-target-dir / 'release' / name
bin-dst := base-dir / 'bin' / name
desktop-src := 'data' / (appid + '.desktop')
desktop-dst := base-dir / 'share' / 'applications' / (appid + '.desktop')
metainfo-src := 'data' / (appid + '.metainfo.xml')
metainfo-dst := base-dir / 'share' / 'metainfo' / (appid + '.metainfo.xml')

# Icons are installed under their hicolor category. The three symbolic glyphs share the
# theme's names on purpose so the host keeps its Cosmic drawing and only the Flatpak
# sandbox falls back to ours (see the task document, T16).
icons-dir := 'data' / 'icons' / 'hicolor' / 'scalable'
icons-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'scalable'
app-icon := (appid + '.svg')

user-base := env('XDG_DATA_HOME', env('HOME') / '.local' / 'share')
user-bin-dst := env('HOME') / '.local' / 'bin' / name
user-apps-dst := user-base / 'applications'
user-metainfo-dst := user-base / 'metainfo' / (appid + '.metainfo.xml')
user-icons-dst := user-base / 'icons' / 'hicolor' / 'scalable'

flatpak-dir := 'flatpak' / appid
manifest := flatpak-dir / (appid + '.json')
flatpak-build-dir := 'build' / 'flatpak'

default: build-release

clean:
    cargo clean

clean-flatpak:
    rm -rf build .flatpak-builder

build-debug *args:
    cargo build {{args}}

build-release *args: (build-debug '--release' args)

check *args:
    cargo clippy --all-targets --all-features {{args}} -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

test *args:
    cargo test {{args}}

validate:
    #!/usr/bin/env bash
    set -euo pipefail
    output="$(desktop-file-validate {{desktop-src}} || true)"
    output="$(printf '%s\n' "${output}" | grep -v '^$' || true)"
    if [ -n "${output}" ]; then printf '%s\n' "${output}"; exit 1; fi
    echo "desktop entry ok"
    appstreamcli validate --no-net {{metainfo-src}}

verify: fmt-check check test validate

run *args:
    cargo run --bin {{name}} {{args}}

install:
    install -Dm0755 {{bin-src}} {{bin-dst}}
    install -Dm0644 {{desktop-src}} {{desktop-dst}}
    install -Dm0644 {{metainfo-src}} {{metainfo-dst}}
    install -Dm0644 {{icons-dir}}/apps/{{app-icon}} {{icons-dst}}/apps/{{app-icon}}
    install -Dm0644 {{icons-dir}}/devices/speaker-symbolic.svg {{icons-dst}}/devices/speaker-symbolic.svg
    install -Dm0644 {{icons-dir}}/devices/audio-headphones-symbolic.svg {{icons-dst}}/devices/audio-headphones-symbolic.svg
    install -Dm0644 {{icons-dir}}/actions/object-select-symbolic.svg {{icons-dst}}/actions/object-select-symbolic.svg

uninstall:
    rm -f {{bin-dst}} {{desktop-dst}} {{metainfo-dst}}
    rm -f {{icons-dst}}/apps/{{app-icon}}
    rm -f {{icons-dst}}/devices/speaker-symbolic.svg
    rm -f {{icons-dst}}/devices/audio-headphones-symbolic.svg
    rm -f {{icons-dst}}/actions/object-select-symbolic.svg

# Install into the current user's data dir (no root). The desktop entry keeps the applet
# discoverable by the panel; its Exec is rewritten to the absolute installed binary so the
# panel does not depend on PATH.
install-user:
    install -Dm0755 {{bin-src}} {{user-bin-dst}}
    install -Dm0644 {{metainfo-src}} {{user-metainfo-dst}}
    install -Dm0644 {{icons-dir}}/apps/{{app-icon}} {{user-icons-dst}}/apps/{{app-icon}}
    install -Dm0644 {{icons-dir}}/devices/speaker-symbolic.svg {{user-icons-dst}}/devices/speaker-symbolic.svg
    install -Dm0644 {{icons-dir}}/devices/audio-headphones-symbolic.svg {{user-icons-dst}}/devices/audio-headphones-symbolic.svg
    install -Dm0644 {{icons-dir}}/actions/object-select-symbolic.svg {{user-icons-dst}}/actions/object-select-symbolic.svg
    mkdir -p {{user-apps-dst}}
    sed 's|^Exec=.*|Exec={{user-bin-dst}}|' {{desktop-src}} > {{user-apps-dst}}/{{appid}}.desktop
    chmod 0644 {{user-apps-dst}}/{{appid}}.desktop
    @echo "Installed. Add 'Audio Output Switch' in Settings -> Desktop -> Panel -> Applets."

uninstall-user:
    rm -f {{user-bin-dst}} {{user-apps-dst}}/{{appid}}.desktop {{user-metainfo-dst}}
    rm -f {{user-icons-dst}}/apps/{{app-icon}}
    rm -f {{user-icons-dst}}/devices/speaker-symbolic.svg
    rm -f {{user-icons-dst}}/devices/audio-headphones-symbolic.svg
    rm -f {{user-icons-dst}}/actions/object-select-symbolic.svg

# Regenerate cargo-sources.json from Cargo.lock with the official generator. Requires
# flatpak/flatpak-builder-tools' flatpak-cargo-generator.py (see the task document, T7).
flatpak-sources:
    python3 flatpak-cargo-generator.py Cargo.lock -o {{flatpak-dir}}/cargo-sources.json

flatpak-build:
    flatpak-builder --user --install --force-clean --ccache \
        {{flatpak-build-dir}} {{manifest}}

# Build against the local working tree instead of the pinned git source, so the manifest can
# be tested before the repository is published.
flatpak-build-local:
    #!/usr/bin/env bash
    set -euo pipefail
    local_manifest="{{flatpak-dir}}/local-test.json"
    trap 'rm -f "${local_manifest}"' EXIT
    python3 -c 'import json,sys; m=json.load(open(sys.argv[1])); m["modules"][0]["sources"][0]={"type":"dir","path":"../..","skip":["target","build",".flatpak-builder",".git","flatpak"]}; json.dump(m,open(sys.argv[2],"w"),indent=2)' \
        "{{manifest}}" "${local_manifest}"
    flatpak-builder --user --install --force-clean --ccache \
        {{flatpak-build-dir}}-local "${local_manifest}"

flatpak-run *args:
    flatpak run {{appid}} {{args}}

flatpak-uninstall:
    flatpak uninstall --user -y {{appid}}
