set_manifest({'id': 'research.sdk_extended', 'name': 'SDK and flow compatibility', 'version': '0.0.1', 'protocol-version': {'major': 0, 'minor': 7}, 'ty-compatibility': {'requirement': '>=0.86.0,<0.87.0'}, 'runtime': {'kind': 'monty', 'artifact': 'plugin.py'}, 'capabilities': {'class-member': True, 'instance-member': True, 'call-state': True, 'mutation-validation': True}, 'claims': {'modules': [{'name': 'demo', 'recursive': False}], 'constructors': [{'kind': 'exact', 'qualified-name': 'demo.Item'}], 'methods': [{'kind': 'exact', 'class-qualified-name': 'demo.Item', 'method-name': 'touch'}], 'attributes': [{'kind': 'exact', 'owner-qualified-name': 'demo.Gateway', 'attribute-name': 'version', 'scope': 'class'}, {'kind': 'exact', 'owner-qualified-name': 'demo.Gateway', 'attribute-name': 'token', 'scope': 'instance'}, {'kind': 'exact', 'owner-qualified-name': 'demo.Gateway', 'attribute-name': 'make', 'scope': 'instance'}, {'kind': 'exact', 'owner-qualified-name': 'demo.Gateway', 'attribute-name': 'api', 'scope': 'class'}], 'mutations': [{'kind': 'exact', 'qualified-name': 'demo.Bag'}]}})

@on_class_member
def api(request: Request) -> Response:
    return member_response(descriptor_member(request["member-name"], type_annotation("int"), class_type=type_annotation("str"), mode="replace-existing"))

@on_class_member_of("version")
def version(request: Request) -> Response:
    return member_response(descriptor_member("version", type_annotation("int"), class_type=type_annotation("str"), mode="replace-existing"))

@on_instance_member_of("token")
def token(request: Request) -> Response:
    return member_response(descriptor_member("token", type_annotation("bytes"), class_type=type_annotation("bool")))

@on_instance_member_of("make")
def make(request: Request) -> Response:
    return member_response(callable_member("make", {"parameters": [parameter("value", type=type_annotation("str"), required=True)], "return-type":type_annotation("int")}, type_annotation("object")))

@on_call_state_of("demo.Item")
def construct(request: Request) -> Response:
    return call_state_patch(result_members={"id":type_annotation("None")}, fresh_result=True, preserves_other_objects=True)

@on_call_state_of("demo.Item.touch")
def touch(request: Request) -> Response:
    return call_state_patch(receiver_members={"id":type_annotation("int")}, preserves_other_objects=True)

@on_mutation_of("item-set")
def mutation(request: Request) -> Response:
    if literal_value(request.get("key") or {}) == "locked":
        return {"kind":"mutation-diagnostics", "diagnostics":[diagnostic("locked-key", "Locked keys cannot be assigned", "error")]}
    return None
