"""django-elasticsearch-dsl smoke test against roll search: a Django model (SQLite) with a
registered Document, the `search_index --rebuild` management command, signal-driven updates and
deletes, and searches through the elasticsearch-dsl query builder (queries, aggregations, scan).

Usage: python smoke.py   (ES_URL, default http://127.0.0.1:59200)
"""

import os
import sys

import django
from django.conf import settings

URL = os.environ.get("ES_URL", "http://127.0.0.1:59200")
settings.configure(
    DEBUG=False,
    INSTALLED_APPS=["django.contrib.contenttypes", "django_elasticsearch_dsl", "shop"],
    DATABASES={"default": {"ENGINE": "django.db.backends.sqlite3", "NAME": ":memory:"}},
    ELASTICSEARCH_DSL={"default": {"hosts": URL, "request_timeout": 120}},
    ELASTICSEARCH_DSL_AUTO_REFRESH=True,
    DEFAULT_AUTO_FIELD="django.db.models.AutoField",
)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
django.setup()

from django.core.management import call_command  # noqa: E402
from django.db import connection  # noqa: E402
from shop.documents import ProductDocument  # noqa: E402
from shop.models import Product  # noqa: E402

failures = []


def check(name):
    def wrap(fn):
        try:
            fn()
            print(f"ok   {name}", flush=True)
        except Exception as e:  # noqa: BLE001
            import traceback

            failures.append(name)
            print(f"FAIL {name}: {type(e).__name__}: {e}", flush=True)
            traceback.print_exc()
        return fn

    return wrap


def eq(got, want, what=""):
    if got != want:
        raise AssertionError(f"{what}: expected {want!r}, got {got!r}")


with connection.schema_editor() as editor:
    editor.create_model(Product)


@check("rebuild index from the database")
def _():
    Product.objects.bulk_create([Product(name=f"product {i} {'red shoe' if i % 2 else 'blue hat'}", color=["red", "blue", "green"][i % 3], price=i) for i in range(40)])
    call_command("search_index", "--rebuild", "-f", verbosity=0)
    ProductDocument._index.refresh()
    eq(ProductDocument.search().count(), 40, "count")


@check("signals index, update and delete")
def _():
    p = Product.objects.create(name="special lamp", color="green", price=999)
    eq(ProductDocument.search().query("match", name="lamp").count(), 1, "created")
    p.price = 1000
    p.save()
    hit = ProductDocument.search().query("match", name="lamp").execute()[0]
    eq(hit.price, 1000, "updated")
    p.delete()
    eq(ProductDocument.search().query("match", name="lamp").count(), 0, "deleted")


@check("queries, aggregations and to_queryset")
def _():
    s = ProductDocument.search().query("match", name="shoe").filter("range", price={"gte": 10}).sort("price")
    s.aggs.bucket("colors", "terms", field="color").metric("avg_price", "avg", field="price")
    r = s[:3].execute()
    eq(r.hits.total.value, 15, "total")
    eq([h.price for h in r], [11, 13, 15], "sorted")
    buckets = {b.key: (b.doc_count, b.avg_price.value) for b in r.aggregations.colors.buckets}
    eq(sum(c for c, _ in buckets.values()), 15, "buckets")
    qs = s[:3].to_queryset()
    eq([p.price for p in qs], [11, 13, 15], "to_queryset")


@check("scan")
def _():
    eq(sum(1 for _ in ProductDocument.search().scan()), 40, "scanned")


@check("delete index")
def _():
    call_command("search_index", "--delete", "-f", verbosity=0)


if failures:
    print(f"django-elasticsearch-dsl: {len(failures)} check(s) failed: {', '.join(failures)}")
    sys.exit(1)
print("django-elasticsearch-dsl: all checks passed")
