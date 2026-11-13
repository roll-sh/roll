from django.db import models


class Product(models.Model):
    name = models.CharField(max_length=100)
    color = models.CharField(max_length=20)
    price = models.IntegerField()

    class Meta:
        app_label = "shop"
