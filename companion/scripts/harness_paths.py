"""Compare diagnostic source identities without changing diagnostic contents."""

from __future__ import annotations

import ntpath
import posixpath
import re
from pathlib import Path, PurePath, PurePosixPath, PureWindowsPath
from urllib.parse import unquote, urlsplit
from urllib.request import url2pathname


def stdlib_locations(value):
    """Compare actual stdlib targets across bundled typeshed revisions.

    Preserve the target module, declaration source, selected characters and all
    project locations. Only typeshed's cache hash and preceding line count vary.
    Reading the returned target also verifies that its URI and range are usable.
    """
    if isinstance(value, list):
        return [stdlib_locations(item) for item in value]
    if not isinstance(value, dict):
        return value
    result = {key: stdlib_locations(item) for key, item in value.items()}
    uri = value.get("uri", "")
    if not isinstance(uri, str):
        return result
    parts = urlsplit(uri)
    match = re.search(r"/vendored/typeshed/[^/]+/stdlib/(.+)$", unquote(parts.path))
    if parts.scheme != "file" or not match or "range" not in value:
        return result
    target = Path(url2pathname(parts.path))
    if parts.netloc:
        target = Path("//" + parts.netloc + str(target))
    lines = target.read_text(encoding="utf-8").splitlines()
    start, end = value["range"]["start"], value["range"]["end"]
    assert 0 <= start["line"] <= end["line"] < len(lines), value
    assert start["line"] != end["line"] or start["character"] <= end["character"], value
    result["uri"] = "typeshed://stdlib/" + match[1]
    result["target_source"] = lines[start["line"] : end["line"] + 1]
    result["range"] = {
        "start": {**start, "line": 0},
        "end": {**end, "line": end["line"] - start["line"]},
    }
    return result


def _ordinary(path: PurePath) -> PurePath:
    if isinstance(path, PureWindowsPath):
        text = str(path)
        if text.startswith("\\\\?\\UNC\\"):
            text = "\\\\" + text[8:]
        elif text.startswith("\\\\?\\"):
            text = text[4:]
            if len(text) < 3 or text[1:3] != ":\\":
                raise ValueError("unsupported Windows device path")
        return PureWindowsPath(ntpath.normpath(text))
    return PurePosixPath(posixpath.normpath(str(path)))


def _resolved(path: PurePath) -> PurePath:
    return _ordinary(path.resolve() if isinstance(path, Path) else path)


def relative_path(
    raw: str, root: PurePath, *, allowed_external: tuple[PurePath, ...] = ()
) -> str:
    """Retain the exact relative filename; reject external or ambiguous paths."""
    path = type(root)(raw)
    if path.drive and not path.root:
        raise ValueError("drive-relative diagnostic path is ambiguous")
    if path.root and not path.drive and root.drive:
        path = type(root)(root.drive + str(path))
    if not path.is_absolute():
        path = root / path
    resolved, base = _resolved(path), _resolved(root)
    try:
        return resolved.relative_to(base).as_posix()
    except ValueError:
        if not any(resolved == _resolved(allowed) for allowed in allowed_external):
            raise
        return resolved.relative_to(base, walk_up=True).as_posix()


def file_uri_matches(uri: str, expected: PurePath) -> bool:
    """Compare a file URI to its expected source, including Windows casing."""
    parts = urlsplit(uri)
    if parts.scheme != "file" or parts.query or parts.fragment:
        raise ValueError("expected a plain file URI")
    text = unquote(parts.path)
    if isinstance(expected, PureWindowsPath):
        if parts.netloc and parts.netloc != "localhost":
            text = "//" + parts.netloc + text
        elif len(text) >= 3 and text[0] == "/" and text[2] == ":":
            text = text[1:]
    elif parts.netloc not in ("", "localhost"):
        raise ValueError("unexpected remote file URI")
    path = type(expected)(text)
    if not path.is_absolute():
        raise ValueError("expected an absolute file URI")
    return _resolved(path) == _resolved(expected)
