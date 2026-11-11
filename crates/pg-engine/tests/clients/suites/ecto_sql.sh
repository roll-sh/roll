# Ecto SQL v3.14.0 (Elixir) with its mix.lock (ecto 3.14.0, postgrex 0.22.2): its Postgres
# integration tests (ECTO_ADAPTER=pg mix test: integration_test/pg, which also runs ecto's own
# integration_test/cases), against PG_URL; the tests make and drop the ecto_test database.
# Results come from a small ExUnit formatter that writes `PASS|FAIL|SKIP <module>::<test>`
# lines (excluded tests are SKIP).
# image: hexpm/elixir:1.20.4-erlang-29.1.1-debian-trixie-20261005-slim
# src: https://github.com/elixir-ecto/ecto_sql/archive/refs/tags/v3.14.0.tar.gz
set -eu
export MIX_HOME=/cache/mix HEX_HOME=/cache/hex MIX_ENV=test LANG=C.UTF-8 ECTO_ADAPTER=pg
export PG_URL="postgres:pw@$PGHOST:$PGPORT"
cat > /cache/home/lst_formatter-ecto_sql.exs <<'EOF'
defmodule LstFormatter do
  use GenServer
  def init(_opts) do
    path = Path.join(System.fetch_env!("OUT"), System.get_env("LST_NAME", "exunit") <> ".lst")
    {:ok, File.open!(path, [:write, :utf8])}
  end
  def handle_cast({:test_finished, %ExUnit.Test{} = t}, f) do
    st = case t.state do
      nil -> "PASS"
      {:failed, _} -> "FAIL"
      {:invalid, _} -> "FAIL"
      _ -> "SKIP"
    end
    name = String.replace(to_string(t.name), ~r/\s+/, " ")
    IO.write(f, "#{st} #{inspect(t.module)}::#{name}\n")
    {:noreply, f}
  end
  def handle_cast({:suite_finished, _}, f), do: (File.close(f); {:noreply, f})
  def handle_cast(_, f), do: {:noreply, f}
end
EOF
cd "$SRC"
[ -d "$MIX_HOME/archives" ] || { mix local.hex --force; mix local.rebar --force; }
mix deps.get > "$OUT/deps.log" 2>&1 || tail -20 "$OUT/deps.log"
LST_NAME=ecto_sql elixir -r /cache/home/lst_formatter-ecto_sql.exs -S mix test \
  --formatter LstFormatter --formatter ExUnit.CLIFormatter "$@" > "$OUT/test.log" 2>&1 || true
tail -5 "$OUT/test.log"
grep -c '^PASS' "$OUT/ecto_sql.lst" || true
grep '^FAIL' "$OUT/ecto_sql.lst" | head -40
