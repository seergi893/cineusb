#!/usr/bin/env python3
"""Prepare the next CineUSB stable release after a merge into main.

Runs only in the trusted main-branch packaging job, never in pull-request builds.
Keeps human-authored release documents when already present. Automatically
created audit text explicitly states the absence of physical-device evidence.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)


def run(*args, check=True):
    return subprocess.run(args, check=check, text=True, capture_output=True)


def version():
    return json.loads(Path("pkg/appinfo.json").read_text(encoding="utf-8"))["version"]


def release_exists(v):
    # GitHub release (not merely a git tag) is the published-version authority.
    p = run("gh", "release", "view", f"cineusb-v{v}", "--json", "tagName", check=False)
    if p.returncode == 0:
        return True
    # Only a genuine 404 means unpublished. Do not silently skip on API/auth errors.
    err = p.stderr.lower()
    if "not found" in err or "404" in err or "no release found" in err:
        return False
    raise RuntimeError(f"Cannot verify release cineusb-v{v}: {p.stderr.strip()}")


def create_documents(v):
    note = Path(f"docs/release-notes/v{v}.md")
    audit = Path(f"docs/release-audits/v{v}.md")
    note.parent.mkdir(parents=True, exist_ok=True)
    audit.parent.mkdir(parents=True, exist_ok=True)
    if not note.exists():
        note.write_text(f"""# CineUSB {v}

Nueva versión estable de CineUSB, publicada tras integrar cambios en la rama principal.

## Cambios

- Incluye las mejoras fusionadas en `main` desde la versión anterior.
- El historial de cambios y las Pull Requests están disponibles en [GitHub](https://github.com/seergi893/cineusb/pulls?q=is%3Apr+is%3Amerged).

## Compatibilidad y limitaciones

La publicación exige que finalicen correctamente las pruebas y la compilación del paquete ARM en GitHub Actions. No se afirma que esta versión se haya probado en un televisor físico. Las características concretas deben verificarse en las Pull Requests fusionadas.

SHA-256 del IPK `__IPK_SHA256__`.
""", encoding="utf-8")
    if not audit.exists():
        audit.write_text(f"""# Auditoría de CineUSB {v}

## Device test evidence

La compilación ARM y las pruebas de GitHub Actions se ejecutan antes de publicar. No se dispone aquí de resultados de pruebas manuales en un televisor LG; cualquier validación física debe añadirse con su evidencia.

## External compatibility reports known at release time

No se documentan nuevos informes externos verificados específicamente para esta versión.

## Compatibility tiers, and what moved

La compatibilidad real con firmwares, discos y formatos no puede inferirse únicamente de una compilación satisfactoria. No se modifica ningún nivel de compatibilidad sin pruebas de hardware.

## Known issues at release, with provenance

Los límites conocidos del sistema de archivos y de la expulsión USB siguen sujetos al comportamiento de webOS. No se afirman correcciones adicionales sin evidencia.

## Deliberately not done

No se inventan resultados de pruebas manuales, compatibilidad de firmware ni mediciones de rendimiento. Esta auditoría es generada como borrador documental y debe ampliarse cuando existan pruebas de dispositivo.

## Release configuration

Paquete estable ARM `com.seergi893.cineusb`, versión {v}, compilado con `RELEASE=1` en GitHub Actions. Se mantienen las verificaciones del paquete.

<!-- BEGIN GENERATED — ci/gen-release-audit.py. Do not edit by hand. -->

Datos de compilación pendientes de publicación.

<!-- END GENERATED -->
""", encoding="utf-8")



def validate_metadata(expected):
    """Validate all tracked version sources, including the actual Cargo.lock package."""
    control = Path("ipkroot/ctl/control").read_text(encoding="utf-8")
    cargo = tomllib.loads(Path("rust-modules/Cargo.toml").read_text(encoding="utf-8"))
    lock = tomllib.loads(Path("rust-modules/Cargo.lock").read_text(encoding="utf-8"))
    if f"Version: {expected}" not in control.splitlines():
        raise SystemExit(f"IPK control version mismatch: expected {expected}")
    if cargo.get("package", {}).get("version") != expected:
        raise SystemExit(f"Cargo.toml version mismatch: expected {expected}")
    matches = [p for p in lock.get("package", []) if p.get("name") == "plxnative-modules"]
    if len(matches) != 1 or matches[0].get("version") != expected:
        actual = [p.get("version") for p in matches]
        raise SystemExit(f"Cargo.lock plxnative-modules version mismatch: expected {expected}, found {actual}")
    print(f"Release metadata is consistent at {expected}")


def main():
    if sys.argv[1:] == ["--check-metadata"]:
        validate_metadata(version())
        return
    if sys.argv[1:]:
        raise SystemExit("Usage: prepare-automatic-release.py [--check-metadata]")
    if os.environ.get("GITHUB_REF") != "refs/heads/main":
        raise SystemExit("Automatic release preparation is allowed only on main")
    current = version()
    if release_exists(current):
        print(f"CineUSB {current} is already published; incrementing minor version")
        run(sys.executable, "ci/bump-version.py", "minor")
    else:
        # An unpublished version may have been prepared by a maintainer already.
        # Refuse a partial bump rather than silently overwriting inconsistent metadata.
        print(f"CineUSB {current} has not been published; retaining its version")
    v = version()
    validate_metadata(v)
    create_documents(v)
    # Lint documents before committing or building.
    for kind, path in (("--lint-note", f"docs/release-notes/v{v}.md"),
                       ("--lint-audit", f"docs/release-audits/v{v}.md")):
        p = subprocess.run([sys.executable, "ci/check-package.py", kind, path])
        if p.returncode:
            raise SystemExit(f"Release document validation failed: {path}")
    print(f"Prepared CineUSB {v}")


if __name__ == "__main__":
    main()
