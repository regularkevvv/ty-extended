#!/usr/bin/env python3
"""Replay original class/project-index contracts with schema-preserving Monty adapters."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import textwrap
from pathlib import Path

from plugin_configuration_differential import diagnostics

NAMES = [
    "class_transform_executes_sdk_plugin_through_in_process_runtime",
    "semantic_project_index_is_passed_to_later_hooks",
    "semantic_project_index_applies_constructor_contributions",
    "semantic_project_index_applies_contributed_field_set_type",
    "semantic_project_index_receives_static_settings_summaries",
    "semantic_plugin_validates_subclass_item_mutations",
    "semantic_plugin_call_summary_preserves_named_boolean_keywords",
    "annotated_cast_preserves_metadata_on_the_result",
    "semantic_project_index_strict_settings_promotes_settings_diagnostics",
    "class_transform_preserves_distinct_field_get_set_and_constructor_types",
    "plugin_field_assignment_uses_instance_set_type",
]

TRANSFORM = """
if not any(base['expression'] == 'toy.Model' for base in request['class'].get('bases', [])):
    return None
fields = []
parameters = []
for item in request['class'].get('fields', []):
    annotation = item.get('annotation')
    if annotation is None:
        continue
    argument = keyword_only(item['name'], annotation, required=not item.get('has-default', False))
    parameters.append(argument)
    fields.append(field(item['name'], annotation, instance_set_type=annotation, constructor_parameter=argument, has_default=item.get('has-default', False)))
return class_patch(fields=fields, constructor=callable_signature(parameters, type_expr('Self')))
"""
SETTINGS = """
strict = request['context']['config']['strict_settings']
assert strict == EXPECT_STRICT, 'strict_settings context'
settings = [item for item in request['settings'] if item['module'] == 'minidjango_settings'][0]
unsupported = settings['diagnostics']
assert len(unsupported) == 1, 'unsupported settings diagnostic count'
assert unsupported[0]['id'] == 'ty.settings.unsupported-value', 'unsupported diagnostic id'
assert unsupported[0]['severity'] == ('error' if strict else 'warning'), 'unsupported diagnostic severity'
assert unsupported[0].get('location') is not None, 'unsupported diagnostic location'
missing = [item for item in request['settings'] if item['module'] == 'missing_settings'][0]
assert len(missing['diagnostics']) == 1, 'missing settings diagnostic count'
assert missing['diagnostics'][0]['id'] == 'ty.settings.module-not-found', 'missing diagnostic id'
assert missing['diagnostics'][0]['severity'] == ('error' if strict else 'warning'), 'missing diagnostic severity'
if not strict:
    model = [item for item in request['classes'] if item['qualified-name'] == 'models.Model'][0]
    assert len(model['methods']) == 1, 'method count'
    method = model['methods'][0]
    assert method['name'] == 'active' and len(method['parameters']) == 2, 'method summary'
    assert method['return-type']['snapshot']['kind'] == 'self-type', 'Self snapshot'
    assignment = [item for item in request['assignments'] if item['qualified-name'] == 'models.manager'][0]
    assert assignment['assigned-value']['kind'] == 'call', 'manager assignment kind'
    assert assignment['assigned-value']['callee']['qualified-name'].endswith('build_manager'), 'manager callee'
    values = settings['values']
    assert [item['name'] for item in values] == ['BASE_APPS','EXTRA_APPS','INSTALLED_APPS','AUTH_USER_MODEL','MINIDJANGO_PK_TYPE','FEATURE_ENABLED','RETRY_CODES','MAX_RETRIES','DATABASES'], 'settings names/order'
    actual = {item['name']: literal_value(item['value']) for item in values}
    assert actual['INSTALLED_APPS'] == ['shared','library','accounts'], 'apps'
    assert actual['AUTH_USER_MODEL'] == 'accounts.User', 'auth model'
    assert actual['MINIDJANGO_PK_TYPE'] == 'uuid', 'pk type'
    assert actual['FEATURE_ENABLED'] is True, 'bool value'
    assert actual['RETRY_CODES'] == (1,2), 'tuple concat'
    assert actual['MAX_RETRIES'] == 3, 'int expression'
    assert actual['DATABASES'] == {'default': {'ENGINE': 'postgresql'}}, 'nested dict'
    assert len(settings['dependencies']) == 2, 'dependency count'
    assert {item['path'].replace('\\\\','/').rsplit('/',1)[-1] for item in settings['dependencies']} == {'base_settings.py','minidjango_settings.py'}, 'dependency paths'
return {'kind':'project-index','plugin-index':{'pk_type':'uuid'} if not strict else None}
"""


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def exact(name):
    return {"kind": "exact", "qualified-name": name}


def specifications(index):
    caps = {}
    claims = {}
    hooks = {}
    extra = ""
    config = {}
    if index == 0:
        caps = {"class-transform": True}
        claims = {"classes": [exact("toy.Model")]}
        hooks["class_transform"] = TRANSFORM
        extra = """from typing import assert_type
from models import Book
book = Book(title="ok")
assert_type(book.title, str)
assert_type(book.pages, int)
Book(title=1)
Book()
Book("positional")
"""
    elif index == 1:
        caps = {
            "project-index": True,
            "class-transform": True,
            "instance-member": True,
            "call-return": True,
        }
        claims = {
            "classes": [exact("toy.Model")],
            "attributes": [
                {
                    "kind": "exact",
                    "scope": "instance",
                    "owner-qualified-name": "models.Model",
                    "attribute-name": "indexed",
                }
            ],
            "functions": [{"qualified-name": "models.indexed_field"}],
        }
        hooks["project_index"] = (
            "assert any(item['qualified-name'] == 'models.Model' for item in request['classes']), 'Model summary'\nreturn project_index(plugin_index={'member_type':'str'})\n"
        )
        check = "assert request['project-index']['member_type'] == 'str', 'project index forwarding'\n"
        hooks["class_transform"] = check + "return class_patch()\n"
        hooks["instance_member"] = (
            check
            + "return member_response(member(request['member-name'], type_annotation('str'), read_only=True))\n"
        )
        hooks["call_return"] = (
            check + "return call_return_patch(type_annotation('str'))\n"
        )
    elif index in (2, 3):
        caps = {"project-index": True}
        owner = "models.Widget" if index == 2 else "models.Target"
        if index == 2:
            patch = "contribution({}, constructor_target('models.Widget'), constructor_contribution(callable_signature([keyword_only('name',type_annotation('str'),required=True)],type_annotation('None'))), 'models.Widget.__init__')"
        else:
            patch = "contribution({}, instance_target('models.Target'), field_contribution(field('contributed',type_annotation('int'),instance_set_type=type_annotation('str'),has_default=True)), 'models.Target.contributed')"
        hooks["project_index"] = (
            f"assert any(item['qualified-name'] == {owner!r} for item in request['classes']), 'target class summary'\nresult = project_index(contributions=[{patch}])\nresult['plugin-index'] = None\nreturn result\n"
        )
    elif index in (4, 8):
        caps = {"project-index": True, "settings-data": True}
        claims = {
            "settings": [
                {"module": "minidjango_settings"},
                {"module": "missing_settings"},
            ]
        }
        config = {"strict_settings": index == 8}
        hooks["project_index"] = SETTINGS.replace("EXPECT_STRICT", repr(index == 8))
        if index == 4:
            caps["class-transform"] = True
            claims["classes"] = [exact("toy.Model")]
            hooks["class_transform"] = (
                "assert request['project-index']['pk_type'] == 'uuid', 'settings index forwarding'\nreturn class_patch()\n"
            )
    elif index == 5:
        caps = {"mutation-validation": True}
        claims = {
            "mutations": [
                {"kind": "subclass-of", "base-qualified-name": "models.Immutable"}
            ]
        }
        hooks["mutation"] = """assert request['operation'] == 'item-set', 'operation'
assert request['receiver']['expression'] == 'models.Child', 'receiver identity'
assert request['key']['value'] == {'kind':'str','value':'name'}, 'key summary'
assert request['value']['value'] == {'kind':'str','value':'Ada'}, 'value summary'
assert request['source']['file-path'].replace('\\\\','/').endswith('/models.py'), 'source file'
return mutation_diagnostics([diagnostic('example.immutable-write','immutable item write')])
"""
    elif index == 6:
        caps = {"call-return": True}
        claims = {"functions": [{"qualified-name": "models.Manager.values_list"}]}
        hooks[
            "call_return"
        ] = """named = [item for item in request['arguments'] if item.get('name') == 'named'][0]
assert named['kind'] == 'keyword', 'named keyword kind'
assert named['value'] == {'kind':'bool','value':True}, 'named keyword literal'
return call_return_patch(type_annotation('object'), diagnostics=[diagnostic('contract-trace','named keyword validated','info')])
"""
    elif index == 7:
        return caps, claims, hooks, extra, config
    elif index == 9:
        caps = {"class-transform": True}
        claims = {"classes": [exact("models.Model")]}
        hooks["class_transform"] = """return class_patch(
    fields=[field('field',type_annotation('int'),instance_set_type=type_annotation('str'),descriptor=member_descriptor(type_annotation('int'),class_type=type_annotation('object'),instance_set_type=type_annotation('str')),constructor_parameter=keyword_only('field',None,required=True))],
    class_members=[member('self_class_member',type_annotation('Self'))],
    instance_members=[member('self_instance_member',type_annotation('list[Self]'))],
    constructor=callable_signature([keyword_only('self_value',type_annotation('Self'),required=False)],type_annotation('None')))
"""
        extra = """from typing import assert_type
from models import Model

def check(value: Model) -> None:
    assert_type(Model.field, object)
    assert_type(value.field, int)
    value.field = "ok"
    value.field = 1
    assert_type(Model.self_class_member, Model)
    assert_type(value.self_instance_member, list[Model])
    Model(self_value=value)
    Model(self_value=1)
"""
    else:
        caps = {"class-transform": True}
        claims = {"classes": [exact("models.Base")]}
        hooks[
            "class_transform"
        ] = """if request['class']['qualified-name'] != 'models.Model':
    return None
return class_patch(fields=[field(name,type_annotation('int'),instance_set_type=type_annotation('str'),descriptor=member_descriptor(type_annotation('int'),class_type=type_annotation('int' if name == 'value' else 'object'),instance_set_type=type_annotation('str')),has_default=True) for name in ['value','virtual']])
"""
        extra = """from typing import assert_type
from models import Model, Child
assert_type(Model.virtual, object)
assert_type(Child.virtual, object)
"""
    return caps, claims, hooks, extra, config


def expected_field_wire(index):
    """Independent literal translation of the pinned Rust response structs."""

    def ty(expression):
        return {"expression": expression, "mode": "annotation"}

    def item(name, class_type=None, *, default=True):
        value = {
            "name": name,
            "mode": "fill-on-miss",
            "instance-get-type": ty("int"),
            "instance-set-type": ty("str"),
            "has-default": default,
        }
        if class_type is not None:
            value["descriptor"] = {
                "kind": "descriptor",
                "class-type": ty(class_type),
                "instance-get-type": ty("int"),
                "instance-set-type": ty("str"),
            }
        return value

    if index == 3:
        patch = {"kind": "field", **item("contributed")}
        return {
            "kind": "project-index",
            "plugin-index": None,
            "contributions": [
                {
                    "source": {},
                    "target": {"kind": "instance", "qualified-name": "models.Target"},
                    "patch": patch,
                    "conflict-key": "models.Target.contributed",
                }
            ],
        }
    if index == 10:
        return {
            "kind": "class-patch",
            "fields": [item("value", "int"), item("virtual", "object")],
        }
    field = item("field", "object", default=False)
    field["constructor-parameter"] = {
        "kind": "keyword-only",
        "required": True,
        "name": "field",
    }

    def member(name, expression):
        return {
            "name": name,
            "mode": "fill-on-miss",
            "access": {"kind": "value", "type-expr": ty(expression)},
            "read-only": False,
        }

    return {
        "kind": "class-patch",
        "fields": [field],
        "class-members": [member("self_class_member", "Self")],
        "instance-members": [member("self_instance_member", "list[Self]")],
        "constructor": {
            "return-type": ty("None"),
            "parameters": [
                {
                    "kind": "keyword-only",
                    "required": False,
                    "name": "self_value",
                    "type-expr": ty("Self"),
                }
            ],
        },
    }


NATIVE_QUIRKS = {
    3: {
        "source": "types/attribute_write.rs:981",
        "behavior": "The contributed setter is a fallback only; the existing contributed read member wins. The original weak count assertion passes while the string write, not the integer write, is rejected.",
    },
    9: {
        "source": "types/class/static_literal.rs:2424",
        "behavior": "A class-transform descriptor supplies its class value before normal instance fallback. The public instance read is object although the internal stored get type is int; exact public diagnostics are retained.",
    },
    10: {
        "source": "types/class/static_literal.rs:2096",
        "behavior": "Own setter lookup returns None at the first class without a patch, before reaching its base. Model accepts str and rejects int; Child inherits ordinary int and rejects str. Original comments suggest otherwise but only count two diagnostics.",
    },
}


def plugin_source(manifest, hooks, root):
    text = "set_manifest(" + repr(manifest) + ")\n"
    failure = {
        "project_index": "project_index(diagnostics=errors)",
        "class_transform": "class_patch(diagnostics=errors)",
        "instance_member": "member_response(member(request['member-name'],type_annotation('object'),diagnostics=errors))",
        "call_return": "call_return_patch(type_annotation('object'),diagnostics=errors)",
        "mutation": "mutation_diagnostics(errors)",
    }
    for hook, body in hooks.items():
        text += f"\n@on_{hook}\ndef hook_{hook}(request):\n    try:\n"
        text += textwrap.indent(textwrap.dedent(body).strip(), "        ") + "\n"
        text += (
            "    except Exception as error:\n        errors = [diagnostic('contract-failure',str(error),'error',location="
            + repr(
                {
                    "file-path": str(
                        root
                        / (
                            "models.py"
                            if (root / "models.py").exists()
                            else "minidjango_settings.py"
                        )
                    ),
                    "start": {"line": 1, "column": 1},
                    "end": {"line": 1, "column": 1},
                }
            )
            + ")]\n"
        )
        text += "        return " + failure[hook] + "\n"
    return text


def main():
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    for name in ("companion", "official", "fork", "output"):
        setattr(args, name, getattr(args, name).resolve())
    args.output.mkdir(parents=True)
    original = (
        Path(__file__).resolve().parents[2]
        / "ruff/crates/ty_python_semantic/src/types/class/static_literal.rs"
    )
    source = original.read_text(encoding="utf-8")
    report = {
        "passed": False,
        "artifact": str(args.companion),
        "sha256": digest(args.companion),
        "original_source": str(original),
        "original_source_sha256": digest(original),
        "source_sha256": digest(Path(__file__)),
        "fork_sha256": digest(args.fork),
        "official_sha256": digest(args.official),
        "cases": [],
        "adapter": "Original write_dedented Python files retained. In-process Rust callbacks translated to Monty with the same claims and response schemas. Internal patch assertions additionally exercised in contract_probe.py; assertions inside callbacks become visible error diagnostics, not swallowed Protocol Error responses. Named-keyword callback has a typing-neutral object return plus info trace to prove execution.",
    }
    environment = {**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"}
    try:
        for index, name in enumerate(NAMES):
            block = re.search(
                r"fn " + name + r"\(\).*?(?=\n    #\[test\]|\Z)", source, re.S
            )
            assert block, name
            files = re.findall(
                r'db\.write_dedented\(\s*"/src/([^\"]+)",\s*r#"(.*?)"#,\s*\)\?;',
                block[0],
                re.S,
            )
            assert files, name
            root = args.output / name
            root.mkdir()
            for file, text in files:
                path = root / file
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(textwrap.dedent(text).strip() + "\n", encoding="utf-8")
            caps, claims, hooks, extra, config = specifications(index)
            if extra:
                (root / "contract_probe.py").write_text(extra, encoding="utf-8")
            manifest = {
                "id": "original.class",
                "name": name,
                "version": "0.0.0",
                "protocol-version": {"major": 0, "minor": 7},
                "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
                "runtime": {"kind": "monty", "artifact": "plugin.py"},
                "capabilities": caps,
                "claims": claims,
            }
            save(root / "plugin.json", manifest)
            (root / "plugin.py").write_text(
                plugin_source(manifest, hooks, root), encoding="utf-8"
            )
            if index in (3, 9, 10):
                # Retain the exact SDK wire response as well as the original
                # Rust schema, so descriptor/setter comparisons cannot be
                # explained by an authoring-helper adaptation.
                sdk = original.parents[4] / "ty_plugin_host/python/ty_plugin_sdk.py"
                namespace = {}
                exec(
                    compile(sdk.read_text(encoding="utf-8"), str(sdk), "exec"),
                    namespace,
                )
                exec(
                    compile(
                        (root / "plugin.py").read_text(encoding="utf-8"),
                        "plugin.py",
                        "exec",
                    ),
                    namespace,
                )
                request = (
                    {
                        "kind": "build-project-index",
                        "classes": [{"qualified-name": "models.Target"}],
                    }
                    if index == 3
                    else {
                        "kind": "analyze-class",
                        "class": {"qualified-name": "models.Model"},
                    }
                )
                wire = json.loads(namespace["__ty_handle__"](json.dumps(request)))
                assert wire["kind"] == (
                    "project-index" if index == 3 else "class-patch"
                ), wire
                expected = expected_field_wire(index)
                assert wire == expected, (name, wire, expected)
                save(root / "wire-response.json", wire)
                save(root / "original-response-schema.json", expected)

            config_text = (
                '[environment]\npython-version="3.13"\n[src]\nexclude=["plugin.py"]\n'
            )
            if hooks:
                config_text += '[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid="original.class"\nruntime="monty"\ntrusted=true\npath="plugin.py"\nmanifest-path="plugin.json"\n'
                if config:
                    config_text += "[plugins.plugin.config]\n" + "".join(
                        f"{key}={str(value).lower()}\n" for key, value in config.items()
                    )
            (root / "ty.toml").write_text(config_text, encoding="utf-8")
            row = {
                "name": name,
                "original_python_sha256": {
                    file: digest(root / file) for file, _ in files
                },
                "observer_sha256": digest(root / "contract_probe.py")
                if extra
                else None,
                "runs": [],
                "native_behavior": NATIVE_QUIRKS.get(index),
                "wire_response_sha256": digest(root / "wire-response.json")
                if index in NATIVE_QUIRKS
                else None,
            }
            report["cases"].append(row)
            baseline = None
            for backend in ("fork", "companion"):
                cmd = [
                    str(getattr(args, backend)),
                    "check",
                    "--project",
                    ".",
                    "--output-format",
                    "gitlab",
                ]
                if backend == "companion":
                    cmd += ["--ty", str(args.official)]
                process = subprocess.run(
                    cmd,
                    cwd=root,
                    env=environment,
                    capture_output=True,
                    text=True,
                    timeout=90,
                )
                (root / f"{backend}.stdout").write_text(
                    process.stdout, encoding="utf-8"
                )
                (root / f"{backend}.stderr").write_text(
                    process.stderr, encoding="utf-8"
                )
                items = (
                    diagnostics(json.loads(process.stdout), root)
                    if process.returncode in (0, 1)
                    else []
                )
                failures = [d for d in items if "contract-failure" in d["description"]]
                if backend == "fork":
                    baseline = (process.returncode, items)
                passed = (
                    process.returncode in (0, 1)
                    and not failures
                    and baseline == (process.returncode, items)
                )
                if index == 7:
                    passed &= process.returncode == 0 and not items
                if index == 6:
                    passed &= any("contract-trace" in d["description"] for d in items)
                row["runs"].append(
                    {
                        "backend": backend,
                        "command": cmd,
                        "exit": process.returncode,
                        "diagnostics": items,
                        "contract_failures": failures,
                        "passed": passed,
                    }
                )
            save(args.output / "summary.json", report)
        report["passed"] = all(
            run["passed"] for row in report["cases"] for run in row["runs"]
        )
    finally:
        save(args.output / "summary.json", report)
    print(
        json.dumps(
            {
                "passed": report["passed"],
                "contracts": len(report["cases"]),
                "failed": [
                    row["name"]
                    for row in report["cases"]
                    if not all(run["passed"] for run in row["runs"])
                ],
            }
        )
    )
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
