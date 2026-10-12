from typing import assert_type, Self
from datetime import date, timedelta


class Metadata:
    pass


class Model:
    def chain(self) -> object:
        return object()

    def snapshot_self(self) -> Self:
        return self.chain()


class Generic[T]:
    pass


def int_value() -> object:
    return object()


def list_value() -> object:
    return object()


def fixed_tuple() -> object:
    return object()


def variable_tuple() -> object:
    return object()


def empty_tuple() -> object:
    return object()


def union_value() -> object:
    return object()


def nested_import() -> object:
    return object()


def annotated_value() -> object:
    return object()


def bound_self() -> object:
    return object()


def absent_self() -> object:
    return object()


def missing_nominal() -> object:
    return object()


def missing_project_class() -> object:
    return object()


def invalid_generic() -> object:
    return object()


def project_class() -> object:
    return object()


def project_generic() -> object:
    return object()


def closed_record() -> object:
    return object()


def open_record() -> object:
    return object()


def extra_record() -> object:
    return object()


def virtual_record() -> object:
    return object()


assert_type(int_value(), int)
assert_type(list_value(), list[str])
assert_type(fixed_tuple(), tuple[int, str])
assert_type(variable_tuple(), tuple[int, *tuple[str, ...], bool])
assert_type(empty_tuple(), tuple[()])
assert_type(union_value(), int | str)
assert_type(nested_import(), tuple[date, list[timedelta]])
assert_type(annotated_value(), int)
assert_type(bound_self(), int)
assert_type(absent_self(), bytes)
assert_type(missing_nominal(), bytes)
assert_type(missing_project_class(), bytes)
assert_type(invalid_generic(), bytes)
assert_type(project_class(), Model)
assert_type(project_generic(), Generic[str])
assert_type(virtual_record().value, int)
assert_type(Model().peer, Model)
assert_type(Model().chain(), Model)


def records() -> None:
    row = closed_record()
    assert_type(row["value"], int)
    assert_type(row.get("optional"), str | None)
    assert_type(row["dash-key"], bool)
    assert_type(row["readonly"], int)
    assert_type(open_record()["value"], int)
    assert_type(extra_record()["custom"], str)
    bad: str = row["value"]


class QuerySet[ModelT, RowT]:
    def map_row(self) -> object:
        return object()


def rows_record() -> object:
    return object()


def rows_tuple() -> object:
    return object()


def rows_named() -> object:
    return object()


assert_type(rows_record().map_row()["title"], str)
assert_type(rows_tuple().map_row(), tuple[str, int])
assert_type(rows_named().map_row().pages, int)


class DefaultGeneric[T = int, U = str]:
    pass


Alias = 42


def missing_external_class() -> object:
    return object()


def nonclass_nominal() -> object:
    return object()


def nongeneric_project() -> object:
    return object()


def generic_defaults_short() -> object:
    return object()


def generic_too_many() -> object:
    return object()


def nominal_tuple() -> object:
    return object()


assert_type(missing_external_class(), bytes)
assert_type(nonclass_nominal(), bytes)
assert_type(nongeneric_project(), bytes)
assert_type(generic_defaults_short(), bytes)
assert_type(generic_too_many(), bytes)
assert_type(nominal_tuple(), tuple)

from typing_extensions import TypedDict, Required, ReadOnly


class DeclaredRecord[T](TypedDict, total=False):
    value: Required[T]
    label: ReadOnly[str]


class ChildRecord(DeclaredRecord[int], closed=True):
    count: int


def inspect_record(value: object) -> object:
    return object()


def verify_declared(record: ChildRecord) -> None:
    assert_type(inspect_record(record), int)


def immutable_record() -> None:
    row = closed_record()
    row["readonly"] = 2
    row["unknown"] = "bad"


from typing import reveal_type

reveal_type(closed_record())


def missing_virtual() -> object:
    return object()


assert_type(missing_virtual(), bytes)


def missing_text() -> object:
    return object()


def missing_text_generic() -> object:
    return object()


unknown_text: int = missing_text()
unknown_generic: str = missing_text_generic()


from typing import Annotated, cast


def inspect_annotated(value: object) -> object:
    return object()


def verify_cast_metadata() -> None:
    value = cast(Annotated[Model, Metadata], Model())
    alias = value
    assert_type(inspect_annotated(alias), int)
    assert_type(inspect_annotated(cast(Annotated[Model, Metadata], Model())), int)
    assert_type(inspect_annotated(value), int)


from typing import Literal


def codec_union_none() -> object:
    return object()


assert_type(codec_union_none(), int | None)


def codec_optional() -> object:
    return object()


assert_type(codec_optional(), str | None)


def codec_union() -> object:
    return object()


assert_type(codec_union(), int | str | None)


def codec_list() -> object:
    return object()


assert_type(codec_list(), list[int])


def codec_dict() -> object:
    return object()


assert_type(codec_dict(), dict[str, int])


def codec_tuple() -> object:
    return object()


assert_type(codec_tuple(), tuple[int, bool])


def codec_variadic() -> object:
    return object()


assert_type(codec_variadic(), tuple[str, ...])


def codec_model() -> object:
    return object()


assert_type(codec_model(), Model)


def codec_model_type() -> object:
    return object()


assert_type(codec_model_type(), type[Model])


def codec_model_union() -> object:
    return object()


assert_type(codec_model_union(), Model | None)


def codec_model_tuple() -> object:
    return object()


assert_type(codec_model_tuple(), tuple[Model, bool])


def codec_manager() -> object:
    return object()


assert_type(codec_manager(), Generic[Model])


def codec_queryset() -> object:
    return object()


assert_type(codec_queryset(), QuerySet[Model, str])


def codec_literal_string() -> object:
    return object()


assert_type(codec_literal_string(), Literal["draft"])


def codec_literal_bool() -> object:
    return object()


assert_type(codec_literal_bool(), Literal[1, True])


def codec_literal_signed() -> object:
    return object()


assert_type(codec_literal_signed(), Literal[-1, 2])


def codec_literal_separators() -> object:
    return object()


assert_type(codec_literal_separators(), Literal["a,b", "x|y"])


def codec_literal_bytes() -> object:
    return object()


def codec_record() -> object:
    return object()


def codec_named() -> object:
    return object()


def codec_class() -> object:
    return object()


def codec_class_queryset() -> object:
    return object()


assert_type(codec_record()["title"], str)
assert_type(codec_record()["pages"], int | None)
assert_type(codec_named().title, str)
assert_type(codec_named().pages, int | None)
assert_type(codec_class().score, int)
assert_type(codec_class_queryset().map_row().score, Literal[1])
reveal_type(codec_literal_bytes())
reveal_type(codec_class())
reveal_type(codec_class_queryset())


class Manager[T]:
    def create(self) -> T:
        raise NotImplementedError


def codec_virtual_row() -> object:
    return object()


def codec_virtual_alias() -> object:
    return object()


def codec_virtual_manager() -> object:
    return object()


assert_type(codec_virtual_row().map_row()["title"], str)
assert_type(codec_virtual_alias().title, str)
assert_type(codec_virtual_manager().create(), Model)
assert_type(codec_virtual_manager().score, int)
assert_type(codec_virtual_manager().plugin_marker, str)
