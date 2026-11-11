# A stand-in for PostgreSQL's PostgreSQL::Test::Cluster (see Utils.pm here): `new` / `init` /
# `start` attach to the server already running at PGDUMP_TAP_HOST:PGDUMP_TAP_PORT, and the
# command methods run the client programs against it.
package PostgreSQL::Test::Cluster;

use strict;
use warnings FATAL => 'all';
use PostgreSQL::Test::Utils ();
use Test::More;

sub new
{
	my ($class, $name) = @_;
	return bless { name => $name }, $class;
}

sub init { }
sub start { }
sub stop { }
sub append_conf { }
sub restart { }
sub reload { }
sub port { return $ENV{PGDUMP_TAP_PORT}; }
sub host { return $ENV{PGDUMP_TAP_HOST}; }
sub name { return $_[0]->{name}; }

# A directory for the test's files (dumps), mounted in the client container.
sub backup_dir
{
	my ($self) = @_;
	$self->{backup_dir} //= PostgreSQL::Test::Utils::tempdir();
	return $self->{backup_dir};
}

sub run_log { my $self = shift; return PostgreSQL::Test::Utils::run_log(@_); }

sub connstr
{
	my ($self, $dbname) = @_;
	my $s = "host=$ENV{PGDUMP_TAP_HOST} port=$ENV{PGDUMP_TAP_PORT}";
	$s .= " dbname=$dbname" if defined $dbname;
	return $s;
}

# psql as PostgreSQL::Test::Cluster runs it: -XAtq, ON_ERROR_STOP unless on_error_stop => 0.
sub psql
{
	my ($self, $dbname, $sql, %params) = @_;
	my @cmd = ('psql', '--no-psqlrc', '--no-align', '--tuples-only', '--quiet',
		'--dbname' => $dbname, '--file' => '-');
	$params{on_error_stop} = 1 unless defined $params{on_error_stop};
	push @cmd, '--variable' => 'ON_ERROR_STOP=1' if $params{on_error_stop};
	push @cmd, @{ $params{extra_params} } if defined $params{extra_params};
	my ($code, $out, $err) = PostgreSQL::Test::Utils::run_client(\@cmd, $sql);
	chomp $out;
	chomp $err;
	${ $params{stdout} } = $out if defined $params{stdout};
	${ $params{stderr} } = $err if defined $params{stderr};
	die "psql failed:\n$err\nwhile running:\n$sql" if $code != 0 && $params{on_error_die};
	return $code;
}

# PGDUMP_TAP_LENIENT=1: a failing statement doesn't end the test; the errors go to stderr
# (`# create error: ...`) and the remaining statements still run, so one unsupported statement
# doesn't hide everything after it.
sub safe_psql
{
	my ($self, $dbname, $sql, %params) = @_;
	my ($stdout, $stderr);
	if ($ENV{PGDUMP_TAP_LENIENT})
	{
		$self->psql($dbname, $sql, %params,
			stdout => \$stdout, stderr => \$stderr, on_error_stop => 0);
		print STDERR "# create error: $_\n" for grep { /ERROR|FATAL/ } split /\n/, $stderr // '';
		return $stdout;
	}
	my $code = $self->psql($dbname, $sql, %params,
		stdout => \$stdout, stderr => \$stderr, on_error_stop => 1);
	die "psql error (exit $code):\n$stderr\nwhile running:\n$sql" if $code != 0;
	return $stdout;
}

sub command_ok { my $self = shift; return PostgreSQL::Test::Utils::command_ok(@_); }
sub command_fails { my $self = shift; return PostgreSQL::Test::Utils::command_fails(@_); }
sub command_like { my $self = shift; return PostgreSQL::Test::Utils::command_like(@_); }
sub command_fails_like { my $self = shift; return PostgreSQL::Test::Utils::command_fails_like(@_); }
sub command_exit_is { my $self = shift; return PostgreSQL::Test::Utils::command_exit_is(@_); }
sub command_checks_all { my $self = shift; return PostgreSQL::Test::Utils::command_checks_all(@_); }
sub command_ok_or_fails_like
{
	my $self = shift;
	return PostgreSQL::Test::Utils::command_ok_or_fails_like(@_);
}

sub issues_sql_like
{
	my ($self, $cmd, $expected_sql, $test_name) = @_;
	ok(0, "$test_name: issues_sql_like needs the server log (not available)");
}

1;
