from typing import Callable, Protocol, assert_type


class IntFunction(Protocol):
    def __call__(self, value: int, /) -> int: ...


class Resource:
    status: str | None = None

    def activate(self) -> None:
        self.status = "ready"

    def release(self) -> None:
        self.status = None

    def local_class_access(self) -> str:
        return self.label.upper()


def decode(value: object) -> object:
    return value


def echo(value: object) -> object:
    return value


def apply(function: object) -> object:
    return function


def unknown(value: object) -> None:
    pass


def exercise(condition: bool) -> None:
    assert_type(decode("42"), int)
    number = decode("42")
    assert_type(echo(number), int)
    assert_type(echo(echo(number)), int)
    assert_type(apply(lambda item: item + 1), int)

    resource = Resource()
    assert_type(resource.label, str)
    assert_type(resource.token, bytes)
    assert_type(resource.status, None)
    alias = resource
    alias.activate()
    assert_type(resource.status, str)
    if condition:
        alias.release()
    assert_type(resource.status, str | None)
    alias.release()
    assert_type(resource.status, None)
    alias.activate()
    unknown(resource)
    assert_type(resource.status, str | None)

    second = Resource()
    resource.activate()
    assert_type(resource.status, str)
    assert_type(second.status, None)
    second.activate()
    assert_type(resource.status, str)


def negative_checks() -> None:
    decode(42)
    apply(lambda item: item + "bad")
    result: str = decode("42")
