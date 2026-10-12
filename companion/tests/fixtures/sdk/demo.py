from typing import assert_type, reveal_type


class Host:
    pass


def verify(host: Host) -> None:
    assert_type(host.item.writable, int)
    assert_type(Host.Carrier.writable, str)
    host.item.writable = "accepted by setter"
    host.item.writable = 1
    host.item.readonly = 1
    host.item.separate_set = "accepted by declared setter"
    reveal_type(Host.Carrier.instance_only)


class Base[T]:
    value: T


class Middle[T](Base[list[T]]):
    pass


class Concrete(Middle[int]):
    pass


def generics(value: Concrete) -> None:
    assert_type(value.value, list[int])
    assert_type(echo(value.value), list[int])


def echo(value: object) -> object:
    return value


import builtins as primitive
from typing import Literal as ExpectedLiteral


def raw() -> primitive.object:
    return None


def shadowing(
    host: Host,
    int: primitive.object,
    str: primitive.object,
    tuple: primitive.object,
    list: primitive.object,
    object: primitive.object,
    type: primitive.object,
    typing: primitive.object,
    typing_extensions: primitive.object,
    Literal: primitive.object,
    Any: primitive.object,
) -> None:
    assert_type(echo(host.item.instance_only), primitive.int)
    assert_type(raw(), primitive.tuple[primitive.list[primitive.int], ExpectedLiteral["typing.int"]])
