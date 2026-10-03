from __future__ import annotations

from typing import reveal_type

import models

# --- class-transform: constructor parameters come from declared fields ---
ada = models.User(name="ada", email=None)
post = models.Post(title="hello", author=ada)

# --- class-transform: contributed members and fields ---
reveal_type(models.User.objects)  # per-model virtual Manager
reveal_type(ada.id)  # int (fill-on-miss field)
reveal_type(ada.email)  # str | None (null=True)
reveal_type(post.author)  # models.User (ForeignKey -> model)
reveal_type(post.author_id)  # int (<fk>_id column)

# --- project-index contributions: reverse relations ---
reveal_type(ada.posts)  # manager of Post, contributed onto User
for related in ada.posts.filter(title__contains="hello"):
    reveal_type(related)  # models.Post

# --- call-return: queryset row typing and lookup validation ---
reveal_type(models.User.objects.filter(name__icontains="ada"))  # QuerySet[User, User]
reveal_type(models.User.objects.get(id=1))  # models.User
reveal_type(models.User.objects.get_or_create(name="ada"))  # tuple[User, bool]
reveal_type(models.User.objects.first())  # models.User | None
reveal_type(models.User.objects.count())  # int
reveal_type(models.User.objects.exists())  # bool

# field-path lookups traverse relations: author -> User.name
reveal_type(models.Post.objects.filter(author__name="ada"))  # QuerySet[Post, Post]

# values / values_list produce shaped rows
reveal_type(models.Post.objects.values_list("title", "views"))  # tuple[str, int | None]
reveal_type(models.Post.objects.values_list("title", flat=True))  # str rows
reveal_type(
    models.Post.objects.values_list("title", "views", named=True)
)  # NamedTuple row
reveal_type(models.Post.objects.values("title"))  # TypedDict row
reveal_type(models.User.objects.annotate(score=1))  # annotated Class() row

# --- plugin diagnostics ---
models.User.objects.filter(nme="ada")  # minidjango.unknown-lookup
models.User.objects.get(
    id="abc"
)  # minidjango.invalid-lookup-value (int field, str value)
