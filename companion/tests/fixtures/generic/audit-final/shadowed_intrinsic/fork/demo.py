from typing import Callable, assert_type
class Resource:
    status: str | None = None
    def activate(self) -> None:
        self.status = "ready"
    def release(self) -> None:
        self.status = None

def example(cast: Callable[[type, object], str]) -> str:
    resource = Resource()
    resource.activate()
    cast(int, resource)
    return resource.status
