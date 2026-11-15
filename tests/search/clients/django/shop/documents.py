from django_elasticsearch_dsl import Document, fields
from django_elasticsearch_dsl.registries import registry

from .models import Product


@registry.register_document
class ProductDocument(Document):
    color = fields.KeywordField()

    class Index:
        name = "clients-django-products"
        settings = {"number_of_shards": 2, "number_of_replicas": 0}

    class Django:
        model = Product
        fields = ["name", "price"]
