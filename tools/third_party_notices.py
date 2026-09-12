"""Generate THIRD-PARTY-NOTICES.txt for the Windows executable.

Run from the repository root whenever dependencies change:

    python tools/third_party_notices.py

It lists every crate compiled into the exe (`cargo tree -e normal` for the
Windows target; build-time tools are not shipped) and every third-party asset
the build embeds, each with the licence text it ships. Crates that ship no
licence file use the text kept in tools/third-party-licenses (see SOURCES.md
there). The file is embedded in the exe and shown on the dashboard's About
page. Its fingerprint line lets a test notice when Cargo.lock has moved on.
"""

import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
VENDORED = ROOT / "tools" / "third-party-licenses"
OUTPUT = ROOT / "THIRD-PARTY-NOTICES.txt"
TARGET = "x86_64-pc-windows-msvc"

# For a choice between licences, the first of these that is offered is used.
PREFERENCE = ["MIT", "Apache-2.0", "BSD-3-Clause", "BSD-2-Clause", "Zlib", "ISC", "0BSD",
              "BSL-1.0", "Unlicense"]
# A phrase every genuine copy of each licence contains, to catch a wrong file.
TELLTALE = {
    "MIT": "Permission is hereby granted",
    "Apache-2.0": "Apache License",
    "BSD-2-Clause": "Redistribution and use",
    "BSD-3-Clause": "Redistribution and use",
    "Zlib": "provided 'as-is'",
    "ISC": "Permission to use, copy, modify, and/or distribute",
    "BSL-1.0": "Boost Software License",
    "Unicode-3.0": "UNICODE",
    "MPL-2.0": "Mozilla Public License",
    "CDLA-Permissive-2.0": "Community Data License Agreement",
}
# The word that identifies each licence's file among a crate's licence files.
FILE_HINT = {
    "MIT": "mit", "Apache-2.0": "apache", "BSD-2-Clause": "bsd", "BSD-3-Clause": "bsd",
    "Zlib": "zlib", "ISC": "isc", "0BSD": "0bsd", "BSL-1.0": "boost", "Unicode-3.0": "unicode",
    "Unlicense": "unlicense",
}
LICENCE_FILE = re.compile(r"(?i)^(licen[cs]e|copying|notice|unlicense)")
# Crates whose package has no licence file, and the folder holding it instead.
VENDORED_FOR = {
    "accesskit": "accesskit",
    "clipboard-win": "clipboard-win",
    "ecolor": "egui", "eframe": "egui", "egui": "egui", "egui-wgpu": "egui",
    "egui-winit": "egui", "egui_extras": "egui", "emath": "egui", "epaint": "egui",
    "enum-map": "enum-map", "enum-map-derive": "enum-map",
    "lucide-icons": "lucide-icons",
    "profiling": "profiling",
}
# Licences whose terms ask binary distributions to say where the source is.
SOURCE_OFFER = {"MPL-2.0"}


def cargo(*args):
    return subprocess.run(["cargo", *args], cwd=ROOT, capture_output=True, text=True,
                          check=True).stdout


def fingerprint(lock_text):
    """FNV-1a 64 over the sorted registry packages of Cargo.lock. The Rust test
    in studio_about.rs computes the same value, so both must stay in step."""
    packages, current = [], {}
    for line in lock_text.replace("\r", "").split("\n") + ["[[package]]"]:
        if line == "[[package]]":
            if current.get("source", "").startswith("registry+"):
                packages.append(f"{current['name']} {current['version']}\n")
            current = {}
            continue
        match = re.match(r'^(name|version|source) = "(.*)"$', line)
        if match:
            current[match.group(1)] = match.group(2)
    value = 0xCBF29CE484222325
    for entry in sorted(packages):
        for byte in entry.encode("utf-8"):
            value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def choose(expression):
    """The licences to reproduce for an SPDX expression: one per OR choice,
    all of an AND. Handles the simple forms crates use, and fails loudly on
    anything else rather than guess."""
    tokens = re.findall(r"\(|\)|[A-Za-z0-9.\-+]+", expression.replace("/", " OR "))
    position = 0

    def term():
        nonlocal position
        if tokens[position] == "(":
            position += 1
            value = either()
            assert tokens[position] == ")", expression
            position += 1
            return value
        position += 1
        return [tokens[position - 1]]

    def both():
        nonlocal position
        value = term()
        while position < len(tokens) and tokens[position] == "AND":
            position += 1
            value = value + term()
        return value

    def either():
        nonlocal position
        options = [both()]
        while position < len(tokens) and tokens[position] == "OR":
            position += 1
            options.append(both())
        if len(options) == 1:
            return options[0]
        ranked = sorted(options, key=lambda option: max(
            PREFERENCE.index(name) if name in PREFERENCE else len(PREFERENCE) for name in option))
        return ranked[0]

    result = either()
    assert position == len(tokens), f"unparsed licence expression: {expression}"
    return result


def reuse_copyright(package_dir):
    """Copyright holders from REUSE `SPDX-FileCopyrightText` source headers."""
    holders = []
    for source in sorted(package_dir.rglob("*.rs")):
        for line in source.read_text(encoding="utf-8", errors="replace").splitlines()[:20]:
            match = re.search(r"SPDX-FileCopyrightText:\s*(.+)", line)
            if match and match.group(1).strip() not in holders:
                holders.append(match.group(1).strip())
    return holders


def licence_texts(package, problems):
    """(licence id, text) pairs for one package, plus any NOTICE files."""
    directory = pathlib.Path(package["manifest_path"]).parent
    chosen = choose(package.get("license") or "")
    if package["name"] in VENDORED_FOR:
        files = sorted(p for p in (VENDORED / VENDORED_FOR[package["name"]]).iterdir())
    else:
        files = sorted(p for p in directory.iterdir() if p.is_file() and LICENCE_FILE.match(p.name))
    texts = []
    used_files = set()
    for licence in chosen:
        hint = FILE_HINT.get(licence)
        candidates = [f for f in files if hint and hint in f.name.lower()]
        if not candidates:
            generic = [f for f in files if not any(h in f.name.lower() for h in FILE_HINT.values())
                       and "notice" not in f.name.lower()]
            candidates = [f for f in generic
                          if TELLTALE.get(licence, "\0") in f.read_text(encoding="utf-8", errors="replace")]
        pointer = [f for f in files if licence == "MIT" and re.search(
            r"\bMIT\b", f.read_text(encoding="utf-8", errors="replace"))]
        if not candidates and pointer:
            # Only a pointer to the licence ships, such as siphasher's COPYING:
            # keep its copyright lines and add the licence's standard terms.
            terms = (VENDORED / "spdx" / "MIT.txt").read_text(encoding="utf-8")
            terms = terms[terms.index("Permission is hereby granted"):].strip()
            pointer_text = pointer[0].read_text(encoding="utf-8", errors="replace").strip()
            texts.append((licence, f"{pointer_text}\n\n{terms}"))
            used_files.add(pointer[0])
            continue
        if not candidates:
            problems.append(f"{package['name']} {package['version']}: no {licence} text among "
                            f"{[f.name for f in files]}")
            continue
        text = candidates[0].read_text(encoding="utf-8", errors="replace").strip()
        used_files.add(candidates[0])
        if TELLTALE.get(licence) and TELLTALE[licence] not in text:
            problems.append(f"{package['name']}: {candidates[0].name} does not look like {licence}")
        if "<copyright holders>" in text:
            holders = reuse_copyright(directory)
            if not holders:
                problems.append(f"{package['name']}: licence template with no copyright holders")
            text = re.sub(r"Copyright \(c\) <year> <copyright holders>",
                          "\n".join(f"Copyright (c) {holder}" for holder in holders), text)
        texts.append((licence, text))
    for notice in [f for f in files if "notice" in f.name.lower()]:
        texts.append(("NOTICE", notice.read_text(encoding="utf-8", errors="replace").strip()))
        used_files.add(notice)
    # Anything else kept for a vendored crate, such as AccessKit's notice for
    # code derived from Chromium, is reproduced too.
    if package["name"] in VENDORED_FOR:
        for extra in files:
            if extra in used_files or extra.suffix == ".md":
                continue
            texts.append((extra.name, extra.read_text(encoding="utf-8", errors="replace").strip()))
    return chosen, texts


def main():
    tree = cargo("tree", "--target", TARGET, "-e", "normal", "--prefix", "none", "--format", "{p}")
    linked = {m.groups() for m in re.finditer(r"^(\S+) v(\S+)", tree, re.M)}
    metadata = json.loads(cargo("metadata", "--format-version", "1"))
    root = metadata["resolve"]["root"]
    by_name = {p["name"]: p for p in metadata["packages"]}
    packages = sorted((p for p in metadata["packages"]
                       if (p["name"], p["version"]) in linked and p["id"] != root),
                      key=lambda p: (p["name"].lower(), p["version"]))

    problems = []
    blocks = {}  # text -> (title, [component lines])
    components = []

    def record(label, licences, texts):
        for licence, text in texts:
            title, users = blocks.setdefault(text, (licence, []))
            users.append(label)

    for package in packages:
        chosen, texts = licence_texts(package, problems)
        label = f"{package['name']} {package['version']}"
        line = f"{label}  -  {package.get('license')}  -  {package.get('repository') or 'crates.io'}"
        if SOURCE_OFFER & set(chosen):
            line += (f"\n    Source code: https://crates.io/crates/{package['name']}/"
                     f"{package['version']}")
        components.append(line)
        record(label, chosen, texts)

    fonts = by_name["epaint_default_fonts"]
    font_dir = pathlib.Path(fonts["manifest_path"]).parent / "fonts"
    assets = [
        ("Ubuntu Light font (subset embedded as the interface fallback font)",
         "Ubuntu Font Licence 1.0", (font_dir / "UFL.txt").read_text(encoding="utf-8").strip()),
        ("Lucide icons (subset of the icon font embedded for the interface)",
         "ISC (Lucide) and MIT (portions from Feather)",
         (VENDORED / "lucide-icons" / "LICENSE-LUCIDE-ICONS").read_text(encoding="utf-8").strip()),
    ]
    for name, licence, text in assets:
        components.append(f"{name}  -  {licence}")
        record(name, [licence], [(licence, text)])

    if problems:
        print("Cannot generate the notices:", *problems, sep="\n  ")
        return 1

    lock = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
    out = [
        "THIRD-PARTY NOTICES",
        "",
        "Claude Code Usage Monitor Pro includes the third-party software and assets listed",
        "below. Each is used under the licence shown; the full licence texts follow the list.",
        "Where a component offers a choice of licences, the one reproduced here is the one used.",
        "",
        f"Dependencies fingerprint: {fingerprint(lock)}",
        "(Generated by tools/third_party_notices.py; a test fails when Cargo.lock no longer",
        "matches, as a reminder to run it again.)",
        "",
        "=" * 78,
        f"COMPONENTS ({len(components)})",
        "=" * 78,
        "",
        *components,
        "",
    ]
    for text, (title, users) in sorted(blocks.items(), key=lambda item: (item[1][0], item[1][1])):
        out += ["=" * 78, title, "Used by: " + ", ".join(users), "=" * 78, "", text, ""]
    OUTPUT.write_text("\n".join(out), encoding="utf-8", newline="\n")
    print(f"wrote {OUTPUT.name}: {len(packages)} crates and {len(assets)} assets, "
          f"{len(blocks)} distinct licence texts, {OUTPUT.stat().st_size:,} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
