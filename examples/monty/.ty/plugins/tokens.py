# The `TYPE_CHECKING` import resolves SDK names for editors; inside Monty the
# block never executes and the names are ambient (injected by the SDK prelude).

import typing

if typing.TYPE_CHECKING:
    from ty.plugin_sdk import (
        Request,
        Response,
        capabilities,
        call_return_patch,
        manifest,
        on_call_return_of,
        set_manifest,
        type_expr,
    )

set_manifest(
    manifest(
        id="example.tokens",
        name="example-tokens",
        version="0.1.0",
        capabilities=capabilities(call_return=True),
        claims={"functions": [{"qualified-name": "example.issue_token"}]},
        runtime={"kind": "monty", "artifact": "tokens.py"},
    )
)


@on_call_return_of("example.issue_token")
def adjust_issue_token(request: Request) -> Response:
    return call_return_patch(type_expr("example.Token", mode="annotation"))
