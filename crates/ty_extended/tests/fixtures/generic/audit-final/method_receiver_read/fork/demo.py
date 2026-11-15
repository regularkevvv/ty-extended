from typing import Callable, assert_type
class Resource:
    status: str | None = None
    def activate(self) -> None:
        self.status = "ready"
    def release(self) -> None:
        self.status = None

def example() -> str:
    resource = Resource()
    resource.activate()
    return resource.status.upper()
