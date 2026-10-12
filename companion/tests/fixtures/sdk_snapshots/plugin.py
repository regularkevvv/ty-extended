set_manifest(
    {
        "id": "audit.snapshots",
        "name": "SDK snapshots",
        "version": "0.0.0",
        "protocol-version": {"major": 0, "minor": 7},
        "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
        "runtime": {"kind": "monty", "artifact": "plugin.py"},
        "capabilities": {
            "call-return": True,
            "project-index": True,
            "class-transform": True,
        },
        "claims": {
            "functions": [
                {"qualified-name": "demo.int_value"},
                {"qualified-name": "demo.list_value"},
                {"qualified-name": "demo.fixed_tuple"},
                {"qualified-name": "demo.variable_tuple"},
                {"qualified-name": "demo.empty_tuple"},
                {"qualified-name": "demo.union_value"},
                {"qualified-name": "demo.nested_import"},
                {"qualified-name": "demo.annotated_value"},
                {"qualified-name": "demo.bound_self"},
                {"qualified-name": "demo.absent_self"},
                {"qualified-name": "demo.missing_nominal"},
                {"qualified-name": "demo.missing_project_class"},
                {"qualified-name": "demo.invalid_generic"},
                {"qualified-name": "demo.project_class"},
                {"qualified-name": "demo.project_generic"},
                {"qualified-name": "demo.closed_record"},
                {"qualified-name": "demo.open_record"},
                {"qualified-name": "demo.extra_record"},
                {"qualified-name": "demo.virtual_record"},
                {"qualified-name": "demo.Model.chain"},
                {"qualified-name": "demo.rows_record"},
                {"qualified-name": "demo.rows_tuple"},
                {"qualified-name": "demo.rows_named"},
                {"qualified-name": "demo.QuerySet.map_row"},
                {"qualified-name": "demo.missing_external_class"},
                {"qualified-name": "demo.nonclass_nominal"},
                {"qualified-name": "demo.nongeneric_project"},
                {"qualified-name": "demo.generic_defaults_short"},
                {"qualified-name": "demo.generic_too_many"},
                {"qualified-name": "demo.nominal_tuple"},
                {"qualified-name": "demo.inspect_record"},
                {"qualified-name": "demo.missing_virtual"},
                {"qualified-name": "demo.missing_text"},
                {"qualified-name": "demo.missing_text_generic"},
                {"qualified-name": "demo.inspect_annotated"},
                {"qualified-name": "demo.codec_virtual_row"},
                {"qualified-name": "demo.codec_virtual_alias"},
                {"qualified-name": "demo.codec_virtual_manager"},
                {"qualified-name": "demo.codec_union_none"},
                {"qualified-name": "demo.codec_optional"},
                {"qualified-name": "demo.codec_union"},
                {"qualified-name": "demo.codec_list"},
                {"qualified-name": "demo.codec_dict"},
                {"qualified-name": "demo.codec_tuple"},
                {"qualified-name": "demo.codec_variadic"},
                {"qualified-name": "demo.codec_model"},
                {"qualified-name": "demo.codec_model_type"},
                {"qualified-name": "demo.codec_model_union"},
                {"qualified-name": "demo.codec_model_tuple"},
                {"qualified-name": "demo.codec_manager"},
                {"qualified-name": "demo.codec_queryset"},
                {"qualified-name": "demo.codec_literal_string"},
                {"qualified-name": "demo.codec_literal_bool"},
                {"qualified-name": "demo.codec_literal_signed"},
                {"qualified-name": "demo.codec_literal_separators"},
                {"qualified-name": "demo.codec_literal_bytes"},
                {"qualified-name": "demo.codec_record"},
                {"qualified-name": "demo.codec_named"},
                {"qualified-name": "demo.codec_class"},
                {"qualified-name": "demo.codec_class_queryset"},
            ],
            "classes": [{"kind": "exact", "qualified-name": "demo.Model"}],
            "modules": [{"name": "demo"}],
        },
    }
)
RESPONSES = {
    "codec_virtual_row": {
        "expression": "demo.QuerySet[demo.Model, virtual.ValuesRow]",
        "mode": "annotation",
    },
    "codec_virtual_alias": {
        "expression": "RowAlias",
        "mode": "annotation",
        "imports": [
            {"module": "virtual", "name": "ValuesListRow", "alias": "RowAlias"}
        ],
    },
    "codec_virtual_manager": {"expression": "virtual.Manager", "mode": "annotation"},
    "codec_union_none": {"expression": "int | None", "mode": "annotation"},
    "codec_optional": {"expression": "typing.Optional[str]", "mode": "annotation"},
    "codec_union": {"expression": "typing.Union[int, str, None]", "mode": "annotation"},
    "codec_list": {"expression": "list[int]", "mode": "annotation"},
    "codec_dict": {"expression": "dict[str, int]", "mode": "annotation"},
    "codec_tuple": {"expression": "tuple[int, bool]", "mode": "annotation"},
    "codec_variadic": {"expression": "tuple[str, ...]", "mode": "annotation"},
    "codec_model": {"expression": "demo.Model", "mode": "annotation"},
    "codec_model_type": {"expression": "type[demo.Model]", "mode": "annotation"},
    "codec_model_union": {"expression": "demo.Model | None", "mode": "annotation"},
    "codec_model_tuple": {
        "expression": "tuple[demo.Model, bool]",
        "mode": "annotation",
    },
    "codec_manager": {"expression": "demo.Generic[demo.Model]", "mode": "annotation"},
    "codec_queryset": {
        "expression": "demo.QuerySet[demo.Model, str]",
        "mode": "annotation",
    },
    "codec_literal_string": {
        "expression": "typing.Literal['draft']",
        "mode": "annotation",
    },
    "codec_literal_bool": {
        "expression": "typing.Literal[1, True]",
        "mode": "annotation",
    },
    "codec_literal_signed": {
        "expression": "typing.Literal[-1, +2]",
        "mode": "annotation",
    },
    "codec_literal_separators": {
        "expression": "typing.Literal['a,b', \"x|y\"]",
        "mode": "annotation",
    },
    "codec_literal_bytes": {
        "expression": "typing.Literal[b'a,b']",
        "mode": "annotation",
    },
    "codec_record": {
        "expression": 'TypedDict({"title": str, "pages": int | None})',
        "mode": "annotation",
    },
    "codec_named": {
        "expression": 'NamedTuple("BookRow", {"title": str, "pages": int | None})',
        "mode": "annotation",
    },
    "codec_class": {
        "expression": 'Class("AnnotatedBook", {"score": int}, demo.Model)',
        "mode": "annotation",
    },
    "codec_class_queryset": {
        "expression": 'demo.QuerySet[demo.Model, Class("MiniDjangoAnnotatedRow", {"score": Literal[1]}, demo.Model)]',
        "mode": "annotation",
    },
    "int_value": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "builtins.int",
            "arguments": [],
        },
    },
    "list_value": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "builtins.list",
            "arguments": [
                {"kind": "nominal", "qualified-name": "builtins.str", "arguments": []}
            ],
        },
    },
    "fixed_tuple": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "tuple",
            "prefix": [
                {"kind": "nominal", "qualified-name": "builtins.int", "arguments": []}
            ],
            "suffix": [
                {"kind": "nominal", "qualified-name": "builtins.str", "arguments": []}
            ],
        },
    },
    "variable_tuple": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "tuple",
            "prefix": [
                {"kind": "nominal", "qualified-name": "builtins.int", "arguments": []}
            ],
            "variadic": {
                "kind": "nominal",
                "qualified-name": "builtins.str",
                "arguments": [],
            },
            "suffix": [
                {"kind": "nominal", "qualified-name": "builtins.bool", "arguments": []}
            ],
        },
    },
    "empty_tuple": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {"kind": "tuple"},
    },
    "union_value": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "union",
            "elements": [
                {"kind": "nominal", "qualified-name": "builtins.int", "arguments": []},
                {"kind": "nominal", "qualified-name": "builtins.str", "arguments": []},
            ],
        },
    },
    "nested_import": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "tuple",
            "prefix": [
                {
                    "kind": "expression",
                    "expression": "Alias",
                    "mode": "annotation",
                    "imports": [
                        {"module": "datetime", "name": "date", "alias": "Alias"}
                    ],
                },
                {
                    "kind": "expression",
                    "expression": "list[Alias]",
                    "mode": "annotation",
                    "imports": [
                        {"module": "datetime", "name": "timedelta", "alias": "Alias"}
                    ],
                },
            ],
        },
    },
    "annotated_value": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "annotated",
            "base": {
                "kind": "nominal",
                "qualified-name": "builtins.int",
                "arguments": [],
            },
            "metadata": [{"qualified-name": "demo.Metadata", "arguments": []}],
        },
    },
    "bound_self": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "self-type",
            "bound": {
                "kind": "nominal",
                "qualified-name": "builtins.int",
                "arguments": [],
            },
        },
    },
    "absent_self": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {"kind": "self-type"},
    },
    "missing_nominal": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "definitely_missing_package_audit.NoClass",
            "arguments": [],
        },
    },
    "missing_project_class": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "demo.Missing",
            "arguments": [],
        },
    },
    "invalid_generic": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "builtins.int",
            "arguments": [
                {"kind": "nominal", "qualified-name": "builtins.str", "arguments": []}
            ],
        },
    },
    "project_class": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "demo.Model",
            "arguments": [],
        },
    },
    "project_generic": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "demo.Generic",
            "arguments": [
                {"kind": "nominal", "qualified-name": "builtins.str", "arguments": []}
            ],
        },
    },
    "closed_record": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "typed-dict",
            "fields": [
                {
                    "name": "value",
                    "type-snapshot": {
                        "kind": "nominal",
                        "qualified-name": "builtins.int",
                        "arguments": [],
                    },
                },
                {
                    "name": "optional",
                    "type-snapshot": {
                        "kind": "nominal",
                        "qualified-name": "builtins.str",
                        "arguments": [],
                    },
                    "required": False,
                },
                {
                    "name": "dash-key",
                    "type-snapshot": {
                        "kind": "nominal",
                        "qualified-name": "builtins.bool",
                        "arguments": [],
                    },
                },
                {
                    "name": "readonly",
                    "type-snapshot": {
                        "kind": "nominal",
                        "qualified-name": "builtins.int",
                        "arguments": [],
                    },
                    "read-only": True,
                },
            ],
            "closed": True,
        },
    },
    "open_record": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "typed-dict",
            "fields": [
                {
                    "name": "value",
                    "type-snapshot": {
                        "kind": "nominal",
                        "qualified-name": "builtins.int",
                        "arguments": [],
                    },
                }
            ],
        },
    },
    "extra_record": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "typed-dict",
            "fields": [
                {
                    "name": "value",
                    "type-snapshot": {
                        "kind": "nominal",
                        "qualified-name": "builtins.int",
                        "arguments": [],
                    },
                }
            ],
            "extra-items": {
                "name": "",
                "type-snapshot": {
                    "kind": "nominal",
                    "qualified-name": "builtins.str",
                    "arguments": [],
                },
                "read-only": True,
            },
            "closed": True,
        },
    },
    "virtual_record": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {"kind": "plugin-class", "identity": "virtual.Record"},
    },
    "chain": {
        "expression": "object",
        "mode": "annotation",
        "snapshot": {
            "kind": "self-type",
            "bound": {
                "kind": "nominal",
                "qualified-name": "builtins.str",
                "arguments": [],
            },
        },
    },
    "rows_record": {
        "expression": 'demo.QuerySet[demo.Model, TypedDict({"title": str})]',
        "mode": "annotation",
    },
    "rows_tuple": {
        "expression": "demo.QuerySet[demo.Model, tuple[str, int]]",
        "mode": "annotation",
    },
    "rows_named": {
        "expression": 'demo.QuerySet[demo.Model, NamedTuple("Row", {"title": str, "pages": int})]',
        "mode": "annotation",
    },
    "missing_external_class": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "datetime.NoSuchClass",
            "arguments": [],
        },
    },
    "nonclass_nominal": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "demo.Alias",
            "arguments": [],
        },
    },
    "nongeneric_project": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "demo.Model",
            "arguments": [
                {"kind": "nominal", "qualified-name": "builtins.int", "arguments": []}
            ],
        },
    },
    "generic_defaults_short": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "demo.DefaultGeneric",
            "arguments": [
                {"kind": "nominal", "qualified-name": "builtins.int", "arguments": []}
            ],
        },
    },
    "generic_too_many": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "demo.Generic",
            "arguments": [
                {"kind": "nominal", "qualified-name": "builtins.int", "arguments": []},
                {"kind": "nominal", "qualified-name": "builtins.str", "arguments": []},
            ],
        },
    },
    "nominal_tuple": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {
            "kind": "nominal",
            "qualified-name": "builtins.tuple",
            "arguments": [
                {"kind": "nominal", "qualified-name": "builtins.str", "arguments": []}
            ],
        },
    },
    "missing_text": {"expression": "demo.Absent", "mode": "annotation"},
    "missing_text_generic": {"expression": "demo.Absent[int]", "mode": "annotation"},
    "missing_virtual": {
        "expression": "bytes",
        "mode": "annotation",
        "snapshot": {"kind": "plugin-class", "identity": "demo.MissingVirtual"},
    },
}


@on_project_index
def index(request):
    return project_index(
        virtual_types=[
            {
                "name": "virtual.ValuesRow",
                "shape": {
                    "kind": "typed-dict",
                    "fields": [
                        {
                            "name": "title",
                            "type-expr": {"expression": "str", "mode": "annotation"},
                            "required": True,
                            "read-only": False,
                        }
                    ],
                    "total": True,
                },
            },
            {
                "name": "virtual.ValuesListRow",
                "shape": {
                    "kind": "named-tuple",
                    "fields": [
                        {
                            "name": "title",
                            "type-expr": {"expression": "str", "mode": "annotation"},
                            "required": True,
                            "read-only": False,
                        }
                    ],
                },
            },
            {
                "name": "virtual.Manager",
                "shape": {
                    "kind": "class",
                    "bases": [
                        {"expression": "demo.Manager[demo.Model]", "mode": "expression"}
                    ],
                    "members": [
                        {
                            "name": "plugin_marker",
                            "read-only": True,
                            "access": {
                                "kind": "value",
                                "type-expr": {
                                    "expression": "str",
                                    "mode": "annotation",
                                },
                            },
                        },
                        {
                            "name": "score",
                            "read-only": True,
                            "access": {
                                "kind": "descriptor",
                                "instance-get-type": {
                                    "expression": "int",
                                    "mode": "annotation",
                                },
                            },
                        },
                    ],
                },
            },
            virtual_type(
                "virtual.Record",
                virtual_class(members=[member("value", type_annotation("int"))]),
            ),
        ]
    )


@on_class_transform
def transform(request):
    return {
        "kind": "class-patch",
        "instance-members": [
            {
                "name": "peer",
                "access": {
                    "kind": "value",
                    "type-expr": {
                        "expression": "object",
                        "mode": "annotation",
                        "snapshot": {"kind": "self-type"},
                    },
                },
            }
        ],
    }


@on_call_return
def returns(request):
    name = request["callee"]["expression"].split(".")[-1]
    if name == "inspect_annotated":
        snapshot = request["arguments"][0].get("type-expr", {}).get("snapshot", {})
        metadata = snapshot.get("metadata", [])
        valid = (
            snapshot.get("kind") == "annotated"
            and snapshot.get("base", {}).get("qualified-name") == "demo.Model"
            and len(metadata) == 1
            and metadata[0].get("qualified-name") == "<class 'Metadata'>"
        )
        return {
            "kind": "call-return-patch",
            "return-type": type_annotation("int" if valid else "bytes"),
        }
    if name == "inspect_record":
        snapshot = request["arguments"][0].get("type-expr", {}).get("snapshot", {})
        fields = {field["name"]: field for field in snapshot.get("fields", [])}
        valid = (
            snapshot.get("kind") == "typed-dict"
            and snapshot.get("closed") == True
            and len(fields) == 3
            and fields["value"].get("required", True) == True
            and fields["value"]["type-snapshot"].get("qualified-name") == "builtins.int"
            and fields["label"].get("required", True) == False
            and fields["label"].get("read-only", False) == True
            and fields["count"].get("required", True) == True
        )
        return {
            "kind": "call-return-patch",
            "return-type": type_annotation("int" if valid else "bytes"),
        }
    if name == "map_row":
        arguments = request.get("receiver", {}).get("generic-arguments", [])
        if len(arguments) != 2:
            return no_change()
        snapshot = arguments[1].get("snapshot")
        if snapshot is None:
            return {
                "kind": "call-return-patch",
                "return-type": type_annotation("bytes"),
            }
        return {
            "kind": "call-return-patch",
            "return-type": {
                "expression": "object",
                "mode": "annotation",
                "snapshot": snapshot,
            },
        }
    return {"kind": "call-return-patch", "return-type": RESPONSES[name]}
