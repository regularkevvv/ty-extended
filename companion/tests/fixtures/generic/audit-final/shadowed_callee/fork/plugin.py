# This research plugin uses the unmodified ty Python SDK ambient inside Monty.

set_manifest(manifest(
    id="research.generic", name="Generic external projection research", version="0.0.1",
    ty_compatibility=">=0.86.0,<0.87.0",
    capabilities=capabilities(class_transform=True, instance_member=True, call_signature=True,
                              call_return=True, call_state=True, mutation_validation=True),
    claims=claims(
        modules=[module_claim("demo")],
        classes=[class_claim_exact("demo.Resource")],
        constructors=[class_claim_exact("demo.Resource")],
        functions=[symbol_claim("demo.decode"), symbol_claim("demo.apply"), symbol_claim("demo.echo")],
        methods=[method_claim_exact("demo.Resource", "activate"), method_claim_exact("demo.Resource", "release")],
        attributes=[attribute_claim_exact("demo.Resource", "token", "instance")],
        mutations=[class_claim_exact("demo.Resource")],
    ),
))

@on_class_transform_of("demo.Resource")
def resource_class(request: Request) -> Response:
    return class_patch(fields=[field("label", type_annotation("str"))])


@on_call_signature_of("demo.decode")
def decode_signature(request: Request) -> Response:
    return call_signature_patch({
        "parameters": [parameter("value", type=type_annotation("str"), required=True)],
        "return-type": type_annotation("object"),
    })


@on_call_signature_of("demo.apply")
def apply_signature(request: Request) -> Response:
    return call_signature_patch({
        "parameters": [parameter("function", type=type_annotation("IntFunction", imports=[import_binding("demo", "IntFunction")]), required=True)],
        "return-type": type_annotation("int"),
    })


@on_call_return_of("demo.decode")
def decode_return(request: Request) -> Response:
    return call_return_patch(type_annotation("int"))


@on_call_return_of("demo.echo")
def echo_return(request: Request) -> Response:
    args = positional_arguments(request)
    if args and argument_type(args[0]):
        return call_return_patch(argument_type(args[0]))
    return None


@on_call_state_of("demo.Resource")
def resource_created(request: Request) -> Response:
    return call_state_patch(
        result_members={"status": type_annotation("None")},
        fresh_result=True,
        preserves_other_objects=True,
    )


@on_call_state_of("demo.Resource.activate")
def resource_activated(request: Request) -> Response:
    return call_state_patch(receiver_members={"status": type_annotation("str")}, preserves_other_objects=True)


@on_call_state_of("demo.Resource.release")
def resource_released(request: Request) -> Response:
    return call_state_patch(receiver_members={"status": type_annotation("None")}, preserves_other_objects=True)


@on_instance_member_of("token")
def resource_token(request: Request) -> Response:
    if (request.get("owner") or {}).get("expression") in ("Resource", "demo.Resource"):
        return member_response(member("token", type_annotation("bytes")))
    return None


@on_mutation_of("item-set")
def read_only_token(request: Request) -> Response:
    key = literal_value(request.get("key") or {})
    if key == "token":
        return {"kind": "mutation-diagnostics", "diagnostics": [diagnostic("immutable-token", "Token entries cannot be assigned", "error")]}
    return None
