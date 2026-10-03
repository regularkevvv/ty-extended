from __future__ import annotations


class Token:
    """An opaque handle produced by `issue_token`."""


def issue_token(value: str):
    """Issue a token for `value`.

    Deliberately left unannotated: the `example.tokens` ty plugin supplies the
    return type, so callers see `example.Token` instead of `Unknown`.
    """
