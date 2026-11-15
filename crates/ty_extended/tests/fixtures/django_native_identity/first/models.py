from django.db import models

class Person(models.Model):
    name = models.CharField(max_length=50)

class Entry(models.Model):
    owner = models.ForeignKey(Person, on_delete=models.CASCADE, related_name="entries")
