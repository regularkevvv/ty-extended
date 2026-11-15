from django.db import models

class Person(models.Model):
    score = models.IntegerField()

class Entry(models.Model):
    owner = models.ForeignKey(Person, on_delete=models.CASCADE, related_name="entries")
