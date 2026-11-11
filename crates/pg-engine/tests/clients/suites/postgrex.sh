# Postgrex v0.22.4 (Elixir): `mix test` (its test/ directory), as its CI runs it, against the
# postgrex_test databases and roles its test/test_helper.exs makes (setup.sql replicates them).
# The image has no psql, so a psql stub on PATH answers test_helper's probes (version 18.6, ssl
# off, wal_level replica: the logical replication, SSL and unix socket tests are excluded on
# both sides) and skips its setup commands. Results come from a small ExUnit formatter that
# writes `PASS|FAIL|SKIP <module>::<test>` lines (excluded tests are SKIP).
# image: hexpm/elixir:1.20.4-erlang-29.1.1-debian-trixie-20261005-slim
# src: https://github.com/elixir-ecto/postgrex/archive/refs/tags/v0.22.4.tar.gz
set -eu
export MIX_HOME=/cache/mix HEX_HOME=/cache/hex MIX_ENV=test LANG=C.UTF-8
B=/cache/home/postgrex-bin
mkdir -p "$B"
cat > "$B/psql" <<'EOF'
#!/bin/sh
case "$*" in
  *"SELECT version()"*) echo " PostgreSQL 18.6 on x86_64-pc-linux-gnu, 64-bit" ;;
  *"SHOW ssl"*) echo " off" ;;
  *"SHOW wal_level"*) echo " replica" ;;
esac
exit 0
EOF
chmod +x "$B/psql"
export PATH="$B:$PATH"
cat > /cache/home/lst_formatter-postgrex.exs <<'EOF'
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
LST_NAME=postgrex elixir -r /cache/home/lst_formatter-postgrex.exs -S mix test \
  --formatter LstFormatter --formatter ExUnit.CLIFormatter "$@" > "$OUT/test.log" 2>&1 || true
tail -5 "$OUT/test.log"
grep -c '^PASS' "$OUT/postgrex.lst" || true
grep '^FAIL' "$OUT/postgrex.lst" | head -40
