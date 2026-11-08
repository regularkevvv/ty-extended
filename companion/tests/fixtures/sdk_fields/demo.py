from typing import Any, Callable, TypeVar, assert_type, overload, reveal_type


class A:
    x = 1


class B:
    x = 2


class Absent:
    pass


class Declared:
    x: int


class Property:
    @property
    def x(self) -> int:
        return 1


class Descriptor:
    def __get__(self, instance: object, owner: type | None = None) -> int:
        return 1


class DataDescriptor:
    x = Descriptor()


class Parent:
    x: int = 1


class Inherited(Parent):
    pass


T = TypeVar("T")


class OwnerDescriptor:
    @overload
    def __get__(self, instance: None, owner: type[T]) -> type[T]: ...
    @overload
    def __get__(self, instance: object, owner: type[T]) -> int: ...
    def __get__(self, instance: object, owner: type[T]) -> object:
        return owner


class OwnerParent:
    x = OwnerDescriptor()


class OwnerInherited(OwnerParent):
    pass


class Method:
    def x(self) -> int:
        return 1


class Constructed:
    x: int


assert_type(A.x, int)
assert_type(A().x, str)
assert_type(B.x, int)
assert_type(B().x, str)
assert_type(Absent().x, str)
assert_type(Declared.x, int)
assert_type(Declared().x, str)
assert_type(Property.x, property)
assert_type(Property().x, str)
assert_type(DataDescriptor.x, int)
assert_type(DataDescriptor().x, str)
assert_type(Inherited.x, int)
assert_type(Inherited().x, str)
assert_type(OwnerInherited.x, type[OwnerInherited])
assert_type(OwnerInherited().x, str)
method: Callable[[Method], int] = Method.x
assert_type(Method().x, str)
assert_type(Constructed(b"ok").x, str)

# Preserve the class-access error, the setter domain, and constructor checking.
Absent.x  # control: absent-class
A().x = 1  # control: setter
Constructed("wrong")  # control: constructor


class Rebound:
    x: int = 1


Earlier = Rebound


class Rebound:
    x: bytes = b"later"


assert_type(Earlier.x, int)
assert_type(Earlier().x, str)
assert_type(Rebound.x, bytes)
assert_type(Rebound().x, str)


class OriginalBase:
    x: int = 1


class OtherBase:
    y: str = "other"


def replace(cls: type[OriginalBase]) -> type[OtherBase]:
    return OtherBase


@replace
class DecoratedParent(OriginalBase):
    pass


class DecoratedChild(DecoratedParent):  # control: dynamic-base
    pass


reveal_type(DecoratedChild.x)  # control: unknown-class
assert_type(DecoratedChild().x, str)


class AnyParent:
    x: Any


class AnyInherited(AnyParent):
    pass


class DynamicMeta(type):
    def __getattr__(cls, name: str) -> Any:
        return None


class DynamicAbsent(metaclass=DynamicMeta):
    pass


assert_type(AnyInherited.x, Any)
assert_type(AnyInherited().x, str)
assert_type(DynamicAbsent().x, str)


class TypedDescriptorParent:
    x: Descriptor = Descriptor()


class TypedDescriptorInherited(TypedDescriptorParent):
    pass


assert_type(TypedDescriptorInherited().x, str)
assert_type(TypedDescriptorInherited.x, int)


class ContainerParent:
    x: list[int] = []


class ContainerInherited(ContainerParent):
    pass


assert_type(ContainerInherited().x, str)
assert_type(ContainerInherited.x, list[int])


def identity(cls: type[T]) -> type[T]:
    return cls


@identity
class IdentityDecoratedParent:
    x: Descriptor = Descriptor()


class IdentityDecoratedInherited(IdentityDecoratedParent):
    pass


assert_type(IdentityDecoratedInherited.x, int)
assert_type(IdentityDecoratedInherited().x, str)
