# Django 5.2.4 (ORM): tests/runtests.py on the PostgreSQL backend (psycopg 3), a subset of
# apps covering the backend, schema editor, migrations, introspection and the query layer.
# image: python:3.12-slim
# src: https://github.com/django/django/archive/refs/tags/5.2.4.tar.gz
set -eu
V=/cache/venv/django
if [ ! -x $V/bin/python ] || [ ! -f $V/.ok ]; then
  python -m venv $V
  $V/bin/pip install -q -e "$SRC" "psycopg[binary]==3.2.9" asgiref==3.9.1 sqlparse==0.5.3 \
    unittest-xml-reporting==3.2.0 tblib==3.1.0 && touch $V/.ok
fi
S=/cache/work/django-settings
mkdir -p $S
cat > $S/test_pg.py <<EOF
DATABASES = {
    "default": {"ENGINE": "django.db.backends.postgresql", "NAME": "a38_django",
                "USER": "postgres", "PASSWORD": "$PGPASSWORD", "HOST": "$PGHOST", "PORT": "$PGPORT"},
    "other": {"ENGINE": "django.db.backends.postgresql", "NAME": "a38_django_other",
              "USER": "postgres", "PASSWORD": "$PGPASSWORD", "HOST": "$PGHOST", "PORT": "$PGPORT"},
}
SECRET_KEY = "django_tests_secret_key"
PASSWORD_HASHERS = ["django.contrib.auth.hashers.MD5PasswordHasher"]
DEFAULT_AUTO_FIELD = "django.db.models.AutoField"
USE_TZ = False
TEST_RUNNER = "xmlrunner.extra.djangotestrunner.XMLTestRunner"
TEST_OUTPUT_DIR = "$OUT"
EOF
cd "$SRC/tests"
APPS=${DJANGO_APPS:-"backends basic queries expressions aggregation model_fields schema migrations transactions introspection inspectdb custom_lookups lookup ordering update delete get_or_create select_for_update bulk_create db_functions indexes constraints model_inheritance many_to_one m2m_through raw_query timezones"}
# (Arguments: test labels and runtests.py options instead of the default apps.)
[ $# -gt 0 ] && APPS=
PYTHONPATH=$S $V/bin/python -W ignore runtests.py --settings=test_pg --parallel 1 --noinput \
  $APPS "$@" > "$OUT/runtests.log" 2>&1
tail -80 "$OUT/runtests.log"
