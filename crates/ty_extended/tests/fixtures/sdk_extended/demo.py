from typing import Literal, assert_type

class Gateway:
    version: object = None

class Item:
    id: int | None
    def touch(self) -> None: pass

class Bag:
    def __getitem__(self, key: str) -> int: return 1
    def __setitem__(self, key: str, value: int) -> None: pass

assert_type(Gateway.version, object)
assert_type(Gateway.api, int)
assert_type(Gateway().token, bytes)
assert_type(Gateway().make("ok"), int)

def branches(item: Item, value: int) -> None:
    match value:
        case 0:
            item.touch()
        case _:
            item.touch()
    assert_type(item.id, int)

def partial(item: Item, value: int) -> None:
    match value:
        case 0:
            item.touch()
    assert_type(item.id, int | None)

def comprehensions(item: Item, others: list[Item]) -> None:
    item.touch()
    before = item
    [(item.touch(), assert_type(item.id, int)) for item in others]
    assert_type(before.id, int)
    (item.touch() for item in others)
    assert_type(before.id, int)
    assert_type(item.id, int)

class Outer:
    current = Item()
    current.touch()
    assert_type(current.id, int)
    class Nested:
        current = Item()
        current.touch()
        assert_type(current.id, int)

bag = Bag()
bag["locked"] = 2
bag["locked"] += 1
bag["open"] += 1

class Derived(Gateway): pass
assert_type(Derived.api, int)
assert_type(Derived().token, bytes)
assert_type(Derived().make("ok"), int)


def identity_and_formatting(item: Item, other: object, condition: bool) -> None:
    item.touch()
    item is other
    assert_type(item.id, int)
    item is not other
    assert_type(item.id, int)
    union = int | str | None
    assert_type(item.id, int)
    f"value={item.id}"
    assert_type(item.id, int)
    assert condition, other.__str__()
    assert_type(item.id, int)
    item == other
    assert_type(item.id, int | None)


def constant_branches(item: Item) -> None:
    if True:
        item.touch()
    assert_type(item.id, int)
    if False:
        item == object()
    assert_type(item.id, int)


def constant_expression_branches(item: Item) -> None:
    True and item.touch()
    assert_type(item.id, int)
    False or item.touch()
    assert_type(item.id, int)
    item.touch() if True else object()
    assert_type(item.id, int)


def known_conditions(item: Item, enabled: Literal[True]) -> None:
    if enabled:
        item.touch()
    assert_type(item.id, int)
    condition = True
    if condition:
        item.touch()
    assert_type(item.id, int)
