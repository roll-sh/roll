# SQLModel 0.0.48 (Python ORM on SQLAlchemy 2.1.6 + Pydantic 2.14.1): its own repository has
# almost no database tests, so this is a pinned driver program over SQLModel with SQLAlchemy's
# asyncio extension on asyncpg 0.32.0 (and one sync Session on psycopg2), plus Alembic 1.20.1
# autogenerate with its async template, which reflects the catalogs. One PASS/FAIL line per check
# in $OUT/sqlmodel.lst:
#   alembic::*   init (autogenerate on an empty database), upgrade, `alembic check` (reflection
#                must match the models exactly), a second model version (add table / column,
#                drop index, widen a varchar), downgrade and upgrade again;
#   reflect::*   the async inspector: columns and types, defaults, keys, expression indexes,
#                check constraints, enums, comments;
#   crud::*, rel::*, txn::*, agg::*, sync::*   CRUD, JSONB / arrays / enums / uuid / numeric,
#                relationships (selectin, joined, many-to-many), savepoints, isolation, SKIP
#                LOCKED, window functions, CTEs, server-side streaming.
# image: python:3.12-slim
set -eu
V=/cache/venv/sqlmodel
if [ ! -x $V/bin/alembic ]; then
  python -m venv $V
  $V/bin/pip install -q sqlmodel==0.0.48 sqlalchemy[asyncio]==2.1.6 pydantic==2.14.1 \
    asyncpg==0.32.0 alembic==1.20.1 psycopg2-binary==2.9.13 greenlet==3.5.6 mako==1.4.3
fi
W=/cache/work/sqlmodel
rm -rf $W && mkdir -p $W && cd $W
export DB_URL="postgresql+asyncpg://postgres:$PGPASSWORD@$PGHOST:$PGPORT/a81_sqlmodel"
export SYNC_URL="postgresql+psycopg2://postgres:$PGPASSWORD@$PGHOST:$PGPORT/a81_sqlmodel"
cat > models.py <<'EOF'
import datetime as dt
import enum
import os
import uuid
from decimal import Decimal
from typing import Optional

from sqlalchemy import (CheckConstraint, Column, DateTime, Enum as SAEnum, Index,
                        String, UniqueConstraint, func, text)
from sqlalchemy.dialects.postgresql import ARRAY, JSONB
from sqlmodel import Field, Relationship, SQLModel

V2 = os.environ.get("MODELS_V2") == "1"


class Rank(str, enum.Enum):
    rookie = "rookie"
    veteran = "veteran"
    legend = "legend"


class HeroPowerLink(SQLModel, table=True):
    hero_id: Optional[int] = Field(default=None, foreign_key="hero.id", primary_key=True,
                                   ondelete="CASCADE")
    power_id: Optional[int] = Field(default=None, foreign_key="power.id", primary_key=True)


class Team(SQLModel, table=True):
    __table_args__ = (UniqueConstraint("name", name="uq_team_name"),
                      {"comment": "teams of heroes"})
    id: Optional[int] = Field(default=None, primary_key=True)
    name: str = Field(max_length=120 if V2 else 80)
    headquarters: str = Field(default="nowhere", sa_column_kwargs={"server_default": "nowhere"})
    budget: Decimal = Field(default=Decimal("0"), max_digits=12, decimal_places=2)
    heroes: list["Hero"] = Relationship(back_populates="team")


class Power(SQLModel, table=True):
    id: Optional[int] = Field(default=None, primary_key=True)
    name: str = Field(index=True, unique=True)
    heroes: list["Hero"] = Relationship(back_populates="powers", link_model=HeroPowerLink)


class Hero(SQLModel, table=True):
    __table_args__ = (CheckConstraint("age IS NULL OR age >= 0", name="ck_hero_age"),
                      Index("ix_hero_lower_name", text("lower(name)")))
    id: Optional[int] = Field(default=None, primary_key=True)
    name: str = Field(index=True)
    secret_name: str = Field(sa_column_kwargs={"comment": "never shown"})
    age: Optional[int] = Field(default=None, index=not V2)
    rank: Rank = Field(default=Rank.rookie, sa_column=Column(
        SAEnum(Rank, name="hero_rank"), nullable=False, server_default="rookie"))
    uid: uuid.UUID = Field(default_factory=uuid.uuid4, unique=True)
    created: Optional[dt.datetime] = Field(default=None, sa_column=Column(
        DateTime(timezone=True), server_default=func.now(), nullable=False))
    tags: list[str] = Field(default_factory=list, sa_column=Column(
        ARRAY(String), nullable=False, server_default="{}"))
    meta: dict = Field(default_factory=dict, sa_column=Column(
        JSONB, nullable=False, server_default=text("'{}'::jsonb")))
    team_id: Optional[int] = Field(default=None, foreign_key="team.id", ondelete="SET NULL")
    team: Optional[Team] = Relationship(back_populates="heroes")
    powers: list[Power] = Relationship(back_populates="heroes", link_model=HeroPowerLink)
    if V2:
        nickname: Optional[str] = Field(default=None, max_length=40)


if V2:
    class Mission(SQLModel, table=True):
        id: Optional[int] = Field(default=None, primary_key=True)
        title: str
        due: Optional[dt.date] = None
        hero_id: int = Field(foreign_key="hero.id", index=True)
EOF
cat > driver.py <<'EOF'
import asyncio
import hashlib
import os
import re
import subprocess
import sys
import traceback

W, V, OUT = os.getcwd(), sys.argv[1], os.environ["OUT"]
LST = open(f"{OUT}/sqlmodel.lst", "w")


def report(name, ok, detail=""):
    LST.write(f"{'PASS' if ok else 'FAIL'} {name}\n")
    LST.flush()
    print(f"{'PASS' if ok else 'FAIL'} {name}" + (f": {detail}" if detail else ""), flush=True)


def shape(name, value):
    """A reflected structure: its id carries a hash of its text, so a difference between the
    two servers shows as one id on each side; the text itself goes to the log."""
    text = repr(value)
    h = hashlib.sha1(text.encode()).hexdigest()[:8]
    report(f"{name}#{h}", True)
    print(f"  {name} = {text}", flush=True)


def alembic(*args, v2=False):
    env = dict(os.environ, PYTHONPATH=W, MODELS_V2="1" if v2 else "0")
    p = subprocess.run([f"{V}/bin/alembic", *args], env=env, capture_output=True, text=True,
                       cwd=W)
    return p.returncode, (p.stdout + p.stderr).strip()


def step(name, rc_out, want=None):
    rc, out = rc_out
    ok = rc == 0 and (want is None or want in out)
    report(name, ok, "" if ok else out[-1500:])
    return ok


def upgrade_ops(path):
    src = open(path).read()
    body = src.split("def upgrade()")[1].split("def downgrade()")[0]
    return re.findall(r"op\.(\w+)\((?:op\.f\()?'([^']*)'(?:\))?(?:, '([^']*)')?", body), src


COMPARE = """
import os, models
from alembic.autogenerate import compare_metadata
from alembic.migration import MigrationContext
from sqlalchemy import create_engine
from sqlmodel import SQLModel
with create_engine(os.environ["SYNC_URL"]).connect() as c:
    print([d for d in compare_metadata(MigrationContext.configure(c), SQLModel.metadata)
           if not (d[0] == "remove_table" and d[1].name == "alembic_version")])
"""


def migrations():
    rc, out = alembic("init", "-t", "async", "migrations")
    if not step("alembic::init", (rc, out)):
        return
    ini = open("alembic.ini").read()
    ini = re.sub(r"(?m)^sqlalchemy\.url = .*$", "sqlalchemy.url = " + os.environ["DB_URL"], ini)
    open("alembic.ini", "w").write(ini)
    env = open("migrations/env.py").read().replace(
        "target_metadata = None",
        "import models  # noqa\nfrom sqlmodel import SQLModel\ntarget_metadata = SQLModel.metadata")
    open("migrations/env.py", "w").write(env)
    mako = open("migrations/script.py.mako").read().replace(
        "import sqlalchemy as sa", "import sqlalchemy as sa\nimport sqlmodel")
    open("migrations/script.py.mako", "w").write(mako)

    if step("alembic::revision_init", alembic("revision", "--autogenerate", "-m", "init",
                                              "--rev-id", "r1")):
        ops, src = upgrade_ops("migrations/versions/r1_init.py")
        tables = sorted(o[1] for o in ops if o[0] == "create_table")
        report("alembic::init_tables", tables == ["hero", "heropowerlink", "power", "team"],
               str(tables))
        idx = sorted(o[1] for o in ops if o[0] == "create_index")
        report("alembic::init_indexes", idx == ["ix_hero_age", "ix_hero_lower_name",
                                                "ix_hero_name", "ix_power_name"], str(idx))
        report("alembic::init_enum", "name='hero_rank'" in src)
    step("alembic::upgrade_r1", alembic("upgrade", "head"))
    step("alembic::check_r1", alembic("check"), "No new upgrade operations detected")
    step("alembic::current_r1", alembic("current"), "r1 (head)")

    if step("alembic::revision_v2", alembic("revision", "--autogenerate", "-m", "v2",
                                            "--rev-id", "r2", v2=True)):
        ops, src = upgrade_ops("migrations/versions/r2_v2.py")
        got = sorted((o[0], o[1], o[2]) for o in ops)
        want = sorted([("create_table", "mission", ""), ("create_index", "ix_mission_hero_id",
                       "mission"), ("add_column", "hero", ""), ("drop_index", "ix_hero_age",
                       ""), ("alter_column", "team", "name")])
        report("alembic::v2_ops", got == want, f"got {got}")
        report("alembic::v2_alter_type", "type_=sa.VARCHAR(length=80)" in src
               or "existing_type=sa.VARCHAR(length=80)" in src, src[-1500:])
    step("alembic::upgrade_r2", alembic("upgrade", "head", v2=True))
    step("alembic::check_r2", alembic("check", v2=True), "No new upgrade operations detected")
    step("alembic::downgrade_r1", alembic("downgrade", "-1", v2=True))
    # `alembic check` wants the head revision; compare the v1 models to the downgraded schema.
    p = subprocess.run([f"{V}/bin/python", "-c", COMPARE], capture_output=True, text=True,
                       env=dict(os.environ, PYTHONPATH=W, MODELS_V2="0"), cwd=W)
    report("alembic::compare_after_downgrade", p.returncode == 0 and p.stdout.strip() == "[]",
           (p.stdout + p.stderr)[-1500:])
    step("alembic::reupgrade_r2", alembic("upgrade", "head", v2=True))
    step("alembic::history", alembic("history", v2=True), "r1 -> r2 (head), v2")


async def check(name, fn, *args):
    try:
        await fn(*args)
        report(name, True)
    except Exception as e:  # noqa: BLE001
        report(name, False, f"{type(e).__name__}: {e}"[:1500])
        traceback.print_exc(limit=3)


migrations()
os.environ["MODELS_V2"] = "1"
sys.path.insert(0, W)
import checks  # noqa: E402

asyncio.run(checks.main(check, shape))
EOF
cat > checks.py <<'EOF'
import datetime as dt
import os
import uuid
from decimal import Decimal

from sqlalchemy import any_, delete, exc, func, inspect, text, update
from sqlalchemy.dialects.postgresql import insert
from sqlalchemy.ext.asyncio import create_async_engine
from sqlalchemy.orm import joinedload, selectinload
from sqlmodel import Session, create_engine, select
from sqlmodel.ext.asyncio.session import AsyncSession

from models import Hero, HeroPowerLink, Mission, Power, Rank, Team

E = create_async_engine(os.environ["DB_URL"])


def S():
    return AsyncSession(E, expire_on_commit=False)


def sqlstate(e):
    orig = getattr(e, "orig", None)
    return getattr(orig, "sqlstate", None) or getattr(getattr(orig, "__cause__", None),
                                                      "sqlstate", None)


async def seed():
    async with S() as s:
        t1, t2 = Team(name="Avengers", budget=Decimal("1234.50")), Team(name="Defenders")
        s.add_all([t1, t2])
        await s.commit()
        heroes = [Hero(name=f"h{i:03}", secret_name=f"s{i}", age=20 + i % 30, team_id=t1.id
                       if i % 2 else t2.id, tags=["a", f"t{i % 3}"], meta={"n": i, "p":
                       "flight" if i % 5 == 0 else "none"}, rank=list(Rank)[i % 3])
                  for i in range(100)]
        s.add_all(heroes)
        await s.commit()
        r = await s.exec(select(func.count()).select_from(Hero))
        assert r.one() == 100
        await s.refresh(t1)
        assert t1.headquarters == "nowhere" and t1.budget == Decimal("1234.50")


async def server_defaults():
    async with S() as s:
        h = Hero(name="dflt", secret_name="x")
        s.add(h)
        await s.commit()
        await s.refresh(h)
        assert h.rank == Rank.rookie and h.tags == [] and h.meta == {}, (h.rank, h.tags, h.meta)
        assert isinstance(h.created, dt.datetime) and h.created.tzinfo is not None, h.created
        assert isinstance(h.uid, uuid.UUID)
        got = (await s.exec(select(Hero).where(Hero.uid == h.uid))).one()
        assert got.id == h.id


async def queries():
    async with S() as s:
        r = (await s.exec(select(Hero.name).where(Hero.age > 47).order_by(Hero.name))).all()
        assert r == ["h028", "h029", "h058", "h059", "h088", "h089"], r
        r = (await s.exec(select(Hero.name).where(Hero.meta["p"].astext == "flight")
                          .order_by(Hero.name.desc()).limit(3))).all()
        assert r == ["h095", "h090", "h085"], r
        r = (await s.exec(select(func.count()).where(Hero.tags.contains(["t1"])))).one()
        assert r == 33, r
        r = (await s.exec(select(func.count()).where("t2" == any_(Hero.tags)))).one()
        assert r == 33, r
        r = (await s.exec(select(func.count()).where(Hero.rank == Rank.legend))).one()
        assert r == 33, r
        r = (await s.exec(select(Team.budget).where(Team.name == "Avengers"))).one()
        assert r == Decimal("1234.50") and isinstance(r, Decimal), r
        r = (await s.exec(select(Hero.name).where(Hero.name.ilike("H00%")).offset(2)
                          .limit(2).order_by(Hero.name))).all()
        assert r == ["h002", "h003"], r


async def dml():
    async with S() as s:
        r = (await s.execute(update(Hero).where(Hero.name == "h001").values(age=Hero.age + 1)
                          .returning(Hero.age))).all()
        assert r == [(22,)], r
        r = await s.execute(delete(Hero).where(Hero.name == "dflt"))
        assert r.rowcount == 1, r.rowcount
        ids = []
        for _ in range(2):
            stmt = insert(Power).values(name="flight").on_conflict_do_update(
                index_elements=["name"], set_={"name": "flight"}).returning(Power.id)
            ids.append((await s.execute(stmt)).scalar_one())
        assert ids[0] == ids[1], ids
        stmt = insert(Power).values(name="flight").on_conflict_do_nothing().returning(Power.id)
        assert (await s.execute(stmt)).all() == []
        await s.commit()


async def violation(model, kwargs, state, needle):
    async with S() as s:
        s.add(model(**kwargs))
        try:
            await s.commit()
        except exc.IntegrityError as e:
            assert sqlstate(e) == state and needle in str(e), (sqlstate(e), str(e))
            return
        raise AssertionError("no error")


async def relations():
    async with S() as s:
        teams = (await s.exec(select(Team).options(selectinload(Team.heroes))
                              .order_by(Team.name))).all()
        assert [(t.name, len(t.heroes)) for t in teams] == [("Avengers", 50), ("Defenders", 50)]
        h = (await s.exec(select(Hero).options(joinedload(Hero.team), selectinload(Hero.powers))
                          .where(Hero.name == "h003"))).one()
        assert h.team.name == "Avengers" and h.powers == []
        p1, p2 = Power(name="strength"), Power(name="speed")
        h.powers.extend([p1, p2])
        s.add(Mission(title="m1", hero_id=h.id, due=dt.date(2026, 1, 2)))
        await s.commit()
    async with S() as s:
        p = (await s.exec(select(Power).options(selectinload(Power.heroes))
                          .where(Power.name == "speed"))).one()
        assert [x.name for x in p.heroes] == ["h003"]
        m = (await s.exec(select(Mission))).one()
        assert m.due == dt.date(2026, 1, 2)
        await s.execute(text("DELETE FROM team WHERE name = 'Defenders'"))
        n = (await s.exec(select(func.count()).where(Hero.team_id.is_(None)))).one()
        assert n == 50, n
        await s.execute(delete(Mission))
        await s.execute(delete(Hero).where(Hero.name == "h003"))
        n = (await s.exec(select(func.count()).select_from(HeroPowerLink))).one()
        assert n == 0, n
        await s.commit()


async def transactions():
    async with S() as s:
        s.add(Power(name="gone"))
        await s.flush()
        await s.rollback()
        assert (await s.exec(select(Power).where(Power.name == "gone"))).all() == []
        s.add(Power(name="kept"))
        try:
            async with s.begin_nested():
                s.add(Power(name="inner"))
                await s.flush()
                raise ValueError("roll back the savepoint")
        except ValueError:
            pass
        await s.commit()
        names = (await s.exec(select(Power.name).where(Power.name.in_(["kept", "inner"])))).all()
        assert names == ["kept"], names
    eng = E.execution_options(isolation_level="SERIALIZABLE")
    async with eng.connect() as c:
        assert (await c.execute(text("SHOW transaction_isolation"))).scalar() == "serializable"
    async with E.connect() as c1, E.connect() as c2:
        await c1.begin()
        await c2.begin()
        first = (await c1.execute(text("SELECT id FROM hero ORDER BY id LIMIT 1 FOR UPDATE"))
                 ).scalar()
        rows = (await c2.execute(text("SELECT id FROM hero ORDER BY id LIMIT 2 FOR UPDATE "
                                      "SKIP LOCKED"))).scalars().all()
        assert first not in rows and len(rows) == 2, (first, rows)
        await c1.rollback()
        await c2.rollback()
    async with E.connect() as c:
        c = await c.execution_options(postgresql_readonly=True)
        await c.begin()
        try:
            await c.execute(text("INSERT INTO power (name) VALUES ('ro')"))
            raise AssertionError("insert in a read-only transaction")
        except exc.DBAPIError as e:
            assert sqlstate(e) == "25006", sqlstate(e)


async def aggregates():
    async with S() as s:
        r = (await s.exec(select(Hero.rank, func.count()).group_by(Hero.rank)
                          .order_by(Hero.rank))).all()
        assert [(k.value, n) for k, n in r] == [("rookie", 33), ("veteran", 33), ("legend", 33)], r
        rn = func.row_number().over(partition_by=Hero.team_id, order_by=Hero.age.desc()
                                    ).label("rn")
        sub = select(Hero.name, Hero.team_id, rn).where(Hero.team_id.is_not(None)).subquery()
        r = (await s.exec(select(sub.c.name).where(sub.c.rn == 1))).all()
        assert len(r) == 1 and r[0] in ("h029", "h059", "h089"), r
        cte = select(func.generate_series(1, 10).label("n")).cte("nums")
        assert (await s.exec(select(func.sum(cte.c.n)))).one() == 55
        for i in range(20):
            r = (await s.exec(select(Hero.name).where(Hero.name == f"h{i + 10:03}"))).one()
            assert r == f"h{i + 10:03}"
    async with E.connect() as c:
        res = await c.stream(text("SELECT id FROM hero ORDER BY id"))
        n = 0
        async for _ in res:
            n += 1
        assert n == 99, n


async def reflection(shape):
    await E.dispose()
    async with E.connect() as c:
        await c.execute(text("CREATE VIEW hero_names AS SELECT name, age FROM hero WHERE age > 30"))
        await c.commit()

        def go(conn):
            i = inspect(conn)
            out = {"tables": sorted(i.get_table_names()), "views": i.get_view_names(),
                   "enums": i.get_enums(), "view_def": i.get_view_definition("hero_names")}
            for t in out["tables"]:
                cols = [(x["name"], str(x["type"].compile(dialect=conn.dialect)), x["nullable"],
                         x.get("default"), x.get("comment"), x.get("autoincrement"))
                        for x in i.get_columns(t)]
                out[f"columns:{t}"] = cols
                out[f"pk:{t}"] = i.get_pk_constraint(t)
                out[f"fks:{t}"] = sorted(i.get_foreign_keys(t), key=lambda x: x["name"] or "")
                out[f"indexes:{t}"] = sorted(i.get_indexes(t), key=lambda x: x["name"])
                out[f"unique:{t}"] = sorted(i.get_unique_constraints(t), key=lambda x: x["name"])
                out[f"checks:{t}"] = i.get_check_constraints(t)
                out[f"comment:{t}"] = i.get_table_comment(t)
            return out

        for k, v in (await c.run_sync(go)).items():
            shape(f"reflect::{k}", v)


async def sync_session():
    eng = create_engine(os.environ["SYNC_URL"])
    with Session(eng) as s:
        s.add(Power(name="sync"))
        s.commit()
        p = s.exec(select(Power).where(Power.name == "sync")).one()
        assert p.id
        assert s.exec(select(func.count()).select_from(Hero)).one() == 99
    eng.dispose()


async def main(check, shape):
    await check("crud::seed", seed)
    await check("crud::server_defaults", server_defaults)
    await check("crud::queries", queries)
    await check("crud::dml", dml)
    await check("crud::check_violation", violation, Hero, {"name": "x", "secret_name": "x",
                "age": -1}, "23514", "ck_hero_age")
    await check("crud::unique_violation", violation, Team, {"name": "Avengers"}, "23505",
                "uq_team_name")
    await check("crud::fk_violation", violation, Mission, {"title": "x", "hero_id": 999999},
                "23503", "mission_hero_id_fkey")
    await check("rel::all", relations)
    await check("txn::all", transactions)
    await check("agg::all", aggregates)
    await check("sync::session", sync_session)
    try:
        await reflection(shape)
    except Exception as e:  # noqa: BLE001
        print("FAIL reflect::all", e, flush=True)
        print("FAIL reflect::all", file=open(os.environ["OUT"] + "/sqlmodel.lst", "a"))
    await E.dispose()
EOF
$V/bin/python driver.py $V
