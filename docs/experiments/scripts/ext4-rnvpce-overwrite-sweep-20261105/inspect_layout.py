#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
import json
from pathlib import Path
import re
import sys


def parse_stat(path: Path) -> dict:
    text = path.read_text(errors="replace")
    size = re.search(r"^.*Size:\s+(\d+)", text, re.M)
    if not size or "EXTENTS:" not in text:
        raise ValueError(f"invalid debugfs stat output: {path}")
    extents = text.split("EXTENTS:", 1)[1]
    return {
        "file": path.stem,
        "size_bytes": int(size.group(1)),
        "data_extents": len(re.findall(r"\((?!ETB\d+\))\d+(?:-\d+)?\):", extents)),
        "extent_index_blocks": len(re.findall(r"\(ETB\d+\):", extents)),
    }


if __name__ == "__main__":
    print(json.dumps([parse_stat(Path(arg)) for arg in sys.argv[1:]], indent=2))
