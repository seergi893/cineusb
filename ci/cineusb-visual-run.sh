#!/usr/bin/env bash
set -euo pipefail
temp=$1
repo=$2
export CINEUSB_SIM_ROOT="$temp/card-library"
export PLXNATIVE_APP_DIR="$temp/card-assets"
mkdir -p "$PLXNATIVE_APP_DIR"
cp -a "$repo/pkg/." "$PLXNATIVE_APP_DIR/"
cp -a "$repo/vendor/ffmpeg-prefix-host/lib"/libav*-plx.so* "$PLXNATIVE_APP_DIR/"
test -f "$PLXNATIVE_APP_DIR/libavformat-plx.so.63"
export PLXNATIVE_WIN=1920x1080
cd "$repo"
for size in 0 1 2; do
    export PLXNATIVE_RUNTIME_DIR="$temp/card-runtime-$size"
    export PLXNATIVE_SHOT="$PLXNATIVE_RUNTIME_DIR/shot.png"
    remote="$PLXNATIVE_RUNTIME_DIR/plxnative-remote"
    rust-modules/target/tvdev/plxnative-sim 127.0.0.1 32400 > "$temp/card-shots/simulator-$size.log" 2>&1 &
    simulator=$!
    trap 'kill "$simulator" 2>/dev/null || true; cp "$PLXNATIVE_RUNTIME_DIR/plxnative-events.log" "$temp/card-shots/events-$size.log" 2>/dev/null || true' EXIT
    for _ in $(seq 1 100); do
        test ! -p "$remote" || break
        kill -0 "$simulator"
        sleep .1
    done
    test -p "$remote"
    exec 3<>"$remote"
    counter=0
    shot() {
        local scene=$1
        counter=$((counter + 1))
        local file="$PLXNATIVE_RUNTIME_DIR/shot-$counter.png"
        printf 'shot ' >&3
        for _ in $(seq 1 100); do
            if test -f "$file" && test "$(tail -c 12 "$file" | od -An -tx1 | tr -d '[:space:]')" = 0000000049454e44ae426082; then
                cp "$file" "$temp/card-shots/$scene.png"
                return
            fi
            kill -0 "$simulator"
            sleep .1
        done
        return 1
    }
    command_keys() {
        printf '%s ' "$@" >&3
        sleep 2
    }
    sleep 5
    grep -q 'ff: bound avformat' "$PLXNATIVE_RUNTIME_DIR/plxnative-events.log"
    if test "$size" = 0; then
        command_keys down
        shot 01-continuar-viendo
        command_keys ck:494,64
        shot 02-peliculas
        command_keys ck:680,64
        shot 03-series
        command_keys ck:176,300
        command_keys ck:176,300
        shot 04-episodios
        command_keys ck:870,64
        shot 05-documentales
        command_keys ck:1080,64
        shot 06-favoritos
    else
        command_keys down
        shot "continuar-preset-$size"
        command_keys ck:494,64
        shot "0$((6 + size))-peliculas-preset-$size"
        command_keys ck:870,64
        shot "1$size-documentales-preset-$size"
        command_keys ck:680,64
        shot "series-preset-$size"
        command_keys ck:176,300
        command_keys ck:176,300
        shot "episodios-preset-$size"
    fi
    exec 3>&-
    kill "$simulator"
    wait "$simulator" || true
    trap - EXIT
done
