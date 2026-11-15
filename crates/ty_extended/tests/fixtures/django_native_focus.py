from typing import assert_type
from benchapp import models
assert_type(models.Item2500().name, str)  # conformance: fields/char-read expect=pass
assert_type(models.Item2500.objects.get(id=1), models.Item2500)  # conformance: managers/get-result expect=pass
assert_type(models.Item2500.objects.filter(name__icontains="a").get(), models.Item2500)  # conformance: managers/filter-result expect=pass
assert_type(models.Item2500.objects.first(), models.Item2500 | None)  # conformance: managers/first-result expect=pass
assert_type(models.Item2500.objects.count(), int)  # conformance: managers/count-result expect=pass
assert_type(models.Item2500().parent, models.Item2499)  # conformance: relations/string-target expect=pass
invalid_fk_id: str = models.Item2500().parent_id  # conformance: relations/string-target-id-domain expect=fail
assert_type(models.Item2499().item2500_set.get(), models.Item2500)  # conformance: relations/reverse-manager expect=pass
assert_type(models.Item2500.objects.values("name").get()["name"], str)  # conformance: rows/values-field expect=pass
assert_type(models.Item2500.objects.values_list("name", flat=True).get(), str)  # conformance: rows/flat-values-list expect=pass
models.Item2500(name="valid", parent_id=1)  # conformance: constructors/valid expect=pass
models.Item2500().name = [1]  # conformance: fields/invalid-write expect=fail
wrong: int = models.Item2500.objects.get(id=1).name  # conformance: fields/wrong-read-assignment expect=fail
models.Item2500(name=["bad"])  # conformance: constructors/invalid-value expect=fail
models.Item2500(missing_name="bad")  # conformance: constructors/unknown-field expect=fail
models.Item2500.objects.create(name=["bad"])  # conformance: creation/invalid-value expect=fail
models.Item2500.objects.filter(missing_name="bad")  # conformance: queries/unknown-field expect=fail
models.Item2500.objects.filter(name__missing="bad")  # conformance: queries/unknown-lookup expect=fail
models.Item2500.objects.order_by("missing_name")  # conformance: queries/unknown-ordering-field expect=fail
models.Item2500.objects.only("missing_name")  # conformance: queries/unknown-selected-field expect=fail
