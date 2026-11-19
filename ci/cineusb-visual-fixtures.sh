#!/usr/bin/env bash
set -euo pipefail
temp=$1
export CINEUSB_SIM_ROOT="$temp/card-library"
mkdir -p "$CINEUSB_SIM_ROOT"/{Movies,Series,Documentals}
mkdir -p "$CINEUSB_SIM_ROOT/Series/Narcos.Mexico.2018.1080p.WEB-DL/Season 3"
mkdir -p "$CINEUSB_SIM_ROOT/Documentals/Nuestro.Planeta.2019.1080p/Season 2"
create_media() {
    local name=$1 size=$2 trc=$3 space=$4 primaries=$5 codec=$6
    ffmpeg -nostdin -v error -f lavfi -i "color=size=$size:rate=1" -f lavfi -i anullsrc=r=48000:cl=stereo -t 3 -c:v libx264 -threads 2 -preset ultrafast -c:a "$codec" -color_trc "$trc" -colorspace "$space" -color_primaries "$primaries" "$temp/$name-source.mkv"
    ffmpeg -nostdin -v error -itsscale 3300 -i "$temp/$name-source.mkv" -i "$temp/$name-source.mkv" -map 0:v -map 1:a -c copy "$temp/$name.mkv"
}
create_media hdr 3840x1608 smpte2084 bt2020nc bt2020 eac3
create_media hd 1920x800 bt709 bt709 bt709 ac3
create_media uhd 4096x1716 smpte2084 bt2020nc bt2020 aac
cp "$temp/hdr.mkv" "$CINEUSB_SIM_ROOT/Movies/La Odisea - 2026 - BDremux 4K UHD - HDR.mkv"
cp "$temp/hd.mkv" "$CINEUSB_SIM_ROOT/Movies/Blade.Runner.2049.1080p.WEB-DL.mkv"
cp "$temp/hd.mkv" "$CINEUSB_SIM_ROOT/Movies/El señor de los anillos El retorno del rey - 2003 - 1080p.mkv"
cp "$temp/hd.mkv" "$CINEUSB_SIM_ROOT/Series/Narcos.Mexico.2018.1080p.WEB-DL/Season 3/Narcos S03E06 - Una noche.mkv"
cp "$temp/hd.mkv" "$CINEUSB_SIM_ROOT/Series/Narcos.Mexico.2018.1080p.WEB-DL/Season 3/Narcos 3x07 - El camino.mkv"
cp "$temp/uhd.mkv" "$CINEUSB_SIM_ROOT/Documentals/Planeta Tierra II - 4K UHD.mkv"
cp "$temp/uhd.mkv" "$CINEUSB_SIM_ROOT/Documentals/Nuestro.Planeta.2019.1080p/Season 2/Nuestro Planeta 2x01 - Océanos.mkv"
cp "$temp/hd.mkv" "$CINEUSB_SIM_ROOT/Documentals/Nuestro.Planeta.2019.1080p/Season 2/Nuestro Planeta 2x02 - Bosques.mkv"
for size in 0 1 2; do
    export runtime="$temp/card-runtime-$size"
    mkdir -p "$runtime"
    printf 'es 0 0 1 0 %s 0 0 0 0 1 0' "$size" > "$runtime/cineusb-settings"
    python3 - <<'PY'
import os
from pathlib import Path
from struct import pack

root = Path(os.environ["CINEUSB_SIM_ROOT"])
runtime = Path(os.environ["runtime"])
series = next((root / "Series").iterdir())
movies = root / "Movies"
documentaries = root / "Documentals"
episode = next(series.glob("Season 3/*Una noche*"))
next_ep = next(series.glob("Season 3/*El camino*"))
movie = next(movies.glob("La Odisea*"))
doc = next(documentaries.glob("Planeta Tierra II*"))
watched = 2**64 - 1
favorite = watched - 1

def progress(minutes, remaining):
    return (1 << 63) | ((60 * (minutes + remaining)) << 43) | (minutes * 60 * 10**6)

for index, (path, value) in enumerate([
    (episode, progress(18, 28)), (next_ep, watched), (movie, progress(62, 48)),
    (doc, progress(45, 35)), (series, favorite), (movie, favorite),
]):
    key = os.fsencode(root) + b"\0" + os.fsencode(path.relative_to(root)) + b"\0"
    if value != favorite:
        stat = path.stat()
        key += pack("<Qq", stat.st_size, stat.st_mtime_ns // 10**9)
    (runtime / f"cineusb-resume-proof-{index}").write_text(f"{key.hex()} {value}")
PY
done
