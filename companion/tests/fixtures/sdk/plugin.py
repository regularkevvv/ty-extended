set_manifest(manifest(
    id="research.sdk", name="Generic SDK semantics", version="0.0.1",
    ty_compatibility=">=0.84.4,<0.85.0",
    capabilities=capabilities(class_transform=True, project_index=True, call_return=True),
    claims=claims(modules=[module_claim("demo")], classes=[class_claim_exact("demo.Host")], functions=[symbol_claim("demo.echo"), symbol_claim("demo.raw")]),
))


@on_project_index
def index(request: Request) -> Response:
    return project_index(virtual_types=[virtual_type("virtual.Carrier", virtual_class(members=[
        descriptor_member("writable", type_annotation("int"),
                          class_type=type_annotation("str"),
                          instance_set_type=type_annotation("str")),
        descriptor_member("readonly", type_annotation("int"), read_only=True),
        descriptor_member("separate_set", type_annotation("int"), instance_set_type=type_annotation("str")),
        descriptor_member("instance_only", type_annotation("int")),
    ]))])


@on_class_transform_of("demo.Host")
def host(request: Request) -> Response:
    return class_patch(
        fields=[field("item", type_annotation("virtual.Carrier"))],
        class_members=[member("Carrier", type_annotation("type[virtual.Carrier]"))],
    )


@on_call_return_of("demo.echo")
def echo(request: Request) -> Response:
    return call_return_patch(argument_type(positional_arguments(request)[0]))


@on_call_return_of("demo.raw")
def raw(request: Request) -> Response:
    return call_return_patch(type_annotation("tuple[list[int], Literal['typing.int']]"))
