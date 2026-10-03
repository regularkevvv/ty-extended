"""A toy ORM-shaped framework, deliberately under-annotated.

The `example.minidjango` plugin supplies the real semantics (typed constructors,
managers, `<fk>_id` columns, reverse relations, queryset rows). `Manager` and
`QuerySet` are generic so the plugin can read model/row arguments off a
receiver's `generic-arguments`. This module exists to be checked, not run.
"""

from __future__ import annotations

from typing import Generic, Iterator, TypeVar

M = TypeVar("M")
R = TypeVar("R")


class Field:
    def __init__(self, *, null: bool = False, default: object = None) -> None:
        self.null = null
        self.default = default


class CharField(Field):
    pass


class IntegerField(Field):
    pass


class ForeignKey(Field):
    def __init__(
        self,
        to: object,
        *,
        null: bool = False,
        related_name: str | None = None,
    ) -> None:
        super().__init__(null=null)
        self.to = to
        self.related_name = related_name


class QuerySet(Generic[M, R]):
    def __iter__(self) -> Iterator[R]:
        raise NotImplementedError

    def filter(self, **lookups: object) -> QuerySet[M, R]:
        raise NotImplementedError

    def get(self, **lookups: object) -> M:
        raise NotImplementedError

    def get_or_create(self, **lookups: object) -> tuple[M, bool]:
        raise NotImplementedError

    def first(self) -> M | None:
        raise NotImplementedError

    def count(self) -> int:
        raise NotImplementedError

    def exists(self) -> bool:
        raise NotImplementedError

    def values(self, *fields: str) -> QuerySet[M, dict[str, object]]:
        raise NotImplementedError

    def values_list(
        self, *fields: str, flat: bool = False, named: bool = False
    ) -> QuerySet[M, object]:
        raise NotImplementedError

    def annotate(self, **annotations: object) -> QuerySet[M, R]:
        raise NotImplementedError


class Manager(Generic[M]):
    def __iter__(self) -> Iterator[M]:
        raise NotImplementedError

    def filter(self, **lookups: object) -> QuerySet[M, M]:
        raise NotImplementedError

    def get(self, **lookups: object) -> M:
        raise NotImplementedError

    def get_or_create(self, **lookups: object) -> tuple[M, bool]:
        raise NotImplementedError

    def first(self) -> M | None:
        raise NotImplementedError

    def count(self) -> int:
        raise NotImplementedError

    def exists(self) -> bool:
        raise NotImplementedError

    def values(self, *fields: str) -> QuerySet[M, dict[str, object]]:
        raise NotImplementedError

    def values_list(
        self, *fields: str, flat: bool = False, named: bool = False
    ) -> QuerySet[M, object]:
        raise NotImplementedError

    def annotate(self, **annotations: object) -> QuerySet[M, M]:
        raise NotImplementedError


class Model:
    """Subclasses list fields as class attributes holding `Field` calls."""
