from typing import assert_type
from first import models as first
from second import models as second

def check():
    person = first.Person.objects.get(id=1)
    assert_type(person.entries.get(id=1), first.Entry)  # conformance: native.identity/reverse-result expect=pass
    assert_type(person.entries.get(id=1).owner.name, str)  # conformance: native.identity/reverse-member expect=pass
    assert_type(second.Person.objects.get(id=1).score, int)  # conformance: native.identity/second-model expect=pass
    assert_type(first.Entry.objects.get(id=1).owner, first.Person)  # conformance: native.identity/first-relation expect=pass
