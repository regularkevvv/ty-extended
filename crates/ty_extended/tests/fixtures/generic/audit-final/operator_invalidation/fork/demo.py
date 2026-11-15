from typing import Callable, assert_type
class Resource:
    status: str | None = None
    def activate(self) -> None:
        self.status = "ready"
    def release(self) -> None:
        self.status = None

class Dangerous:
    def __init__(self, resource: Resource) -> None:
        self.resource = resource
    def __add__(self, value: int) -> int:
        self.resource.release()
        return value
def example(dangerous: Dangerous, resource: Resource) -> str:
    resource = Resource()
    resource.activate()
    dangerous + 1
    return resource.status
