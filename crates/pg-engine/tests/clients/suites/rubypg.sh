# ruby-pg 1.7.0 (Ruby): its spec/ (rspec), built against the image's libpq (Debian bookworm's).
# Its spec helper starts its own server with initdb / pg_ctl (PG::TestingHelpers::PostgresServer,
# no option to use a running one); the script appends a replacement of that class to a copy of
# spec/helpers.rb that points at $PGHOST:$PGPORT instead (libpq takes user and password from
# PGUSER / PGPASSWORD) and makes the a51_rubypg database in place of `test`. Specs that need the
# helper's own cluster (SSL certificates, unix socket, md5 pg_hba user, a second read-only
# server, pg_ctl) fail on both targets.
# image: ruby:3.4.12-bookworm
# src: https://github.com/ged/ruby-pg/archive/refs/tags/v1.7.0.tar.gz
set -eu
export BUNDLE_PATH=/cache/bundle/rubypg BUNDLE_WITHOUT=development BUNDLE_JOBS=4
export GEM_HOME=/cache/bundle/rubypg-gems
W=/cache/work/rubypg
rm -rf $W && mkdir -p $W && cp -a "$SRC/." $W/
cd $W
bundle install --quiet 2>&1 | tail -20
bundle exec rake compile > "$OUT/compile.log" 2>&1 || { tail -30 "$OUT/compile.log"; exit 1; }
for f in spec/pg/*.rb spec/*.rb; do
  sed -i -e 's/dbname: "test"/dbname: "a51_rubypg"/g' -e "s/dbname: 'test'/dbname: 'a51_rubypg'/g" \
    -e 's/:dbname => :test/:dbname => :a51_rubypg/g' -e 's/nil, nil, :test, nil, nil/nil, nil, :a51_rubypg, nil, nil/g' \
    -e 's/PGDATABASE: "test"/PGDATABASE: "a51_rubypg"/g' \
    -e 's#}/test\([?"]\)#}/a51_rubypg\1#g' -e 's#,/test"#,/a51_rubypg"#g' "$f"
done
cat >> spec/helpers.rb <<'EOF'

# a51: the running server under test instead of an initdb / pg_ctl cluster.
class PG::TestingHelpers::PostgresServer
	def initialize(name, port: 23456, postgresql_conf: '')
		@name = name
		@port = port
		@test_dir = TEST_DIRECTORY + "tmp_test_#{@name}"
		@pgdata = @test_dir + 'data'
		@pgdata.mkpath
		@unix_socket = @test_dir.to_s
		@conninfo = "host=localhost port=#{@port} dbname=a51_rubypg"
	end

	def create_test_db
		c = PG.connect(host: 'localhost', port: @port, dbname: 'postgres')
		c.exec('DROP DATABASE IF EXISTS a51_rubypg')
		c.exec('CREATE DATABASE a51_rubypg')
		c.close
	end

	def teardown
	end
end
EOF
cat > a51_formatter.rb <<'EOF'
class A51Formatter
	RSpec::Core::Formatters.register self, :example_passed, :example_failed, :example_pending, :close
	def initialize(_output)
		@out = File.open(ENV.fetch('A51_LST'), 'w')
	end
	def name(n)
		"#{n.example.file_path.sub(%r{^\./}, '')} #{n.example.full_description}"
	end
	def example_passed(n) = @out.puts("PASS #{name(n)}")
	def example_failed(n) = @out.puts("FAIL #{name(n)}")
	def example_pending(n) = @out.puts("SKIP #{name(n)}")
	def close(_) = @out.close
end
EOF
export A51_LST="$OUT/rubypg.lst" PGHOST=localhost
timeout 1800 bundle exec rspec -I lib -r ./a51_formatter.rb --format A51Formatter \
  --format progress --out "$OUT/rspec.log" --format failures --out "$OUT/failures.txt" spec "$@" || true
tail -5 "$OUT/rspec.log"
