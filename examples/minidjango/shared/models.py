from __future__ import annotations

import minidjango
import minidjango_settings


class User(minidjango.Model):
    name = minidjango.CharField()
    email = minidjango.CharField(null=True)


class Post(minidjango.Model):
    title = minidjango.CharField()
    author = minidjango.ForeignKey(User, related_name="posts")
    # Same reverse name as `author` -> the plugin reports a
    # `minidjango.reverse-relation-conflict` diagnostic for this field.
    editor = minidjango.ForeignKey(User, related_name="posts")
    views = minidjango.IntegerField(null=True)


class Comment(minidjango.Model):
    # A settings-symbol relation target — resolves through `AUTH_USER_MODEL`.
    user = minidjango.ForeignKey(minidjango_settings.AUTH_USER_MODEL)
    # A module-relative string target — resolves to `models.Post`.
    post = minidjango.ForeignKey("Post")
    # A self-relation — resolves to `models.Comment`.
    parent = minidjango.ForeignKey("self", null=True)
    # An unknown target — the plugin reports `minidjango.unknown-relation-target`.
    link = minidjango.ForeignKey("models.Missing")
    body = minidjango.CharField()
