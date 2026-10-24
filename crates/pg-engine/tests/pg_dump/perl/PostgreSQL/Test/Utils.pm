# A stand-in for PostgreSQL's src/test/perl/PostgreSQL/Test/Utils.pm, so that the pg_dump TAP
# tests (src/bin/pg_dump/t/*.pl at REL_18_6) run unmodified against a server that is already
# running: the engine or the reference. Client programs (pg_dump, pg_restore, pg_dumpall, psql)
# run in a container of the reference image on the host network (`PGDUMP_TAP_CONTAINER`), so
# both servers are driven by PostgreSQL 18.6's own binaries. `tests/pg_dump.rs` sets:
#
#   PGDUMP_TAP_CONTAINER  the client container
#   PGDUMP_TAP_HOST / PGDUMP_TAP_PORT / PGDUMP_TAP_PASSWORD  the server (user postgres)
#   PGDUMP_TAP_DIR        the temporary directory, mounted at the same path in the container
package PostgreSQL::Test::Utils;

use strict;
use warnings FATAL => 'all';
use Exporter 'import';
use IPC::Open3;
use Symbol 'gensym';
use File::Path qw(make_path);
use Test::More;

our @EXPORT = qw(
  slurp_file check_pg_config run_command
  command_ok command_fails command_exit_is command_like command_fails_like
  command_ok_or_fails_like command_checks_all
  run_log system_log program_help_ok program_version_ok program_options_handling_ok
  $windows_os $use_unix_sockets
);

our $windows_os = 0;
our $use_unix_sockets = 0;
our $timeout_default = 180;
our $is_msys2 = 0;

sub is_msys { return 0; }

my $tmp_count = 0;

sub tempdir
{
	my $base = $ENV{PGDUMP_TAP_DIR} or die "PGDUMP_TAP_DIR not set";
	my $dir = "$base/tmp" . $tmp_count++;
	system('rm', '-rf', $dir);
	make_path($dir);
	return $dir;
}

sub tempdir_short { return tempdir(); }

sub slurp_file
{
	my ($file) = @_;
	# PGDUMP_TAP_LENIENT (see Cluster.pm): a dump that failed reads as empty.
	if (!-e $file && $ENV{PGDUMP_TAP_LENIENT})
	{
		diag("missing output file $file");
		return '';
	}
	open(my $fh, '<', $file) or die "could not read \"$file\": $!";
	local $/;
	my $s = <$fh>;
	close $fh;
	return $s;
}

# The reference image's build (PostgreSQL 18.6, Alpine) has zlib, lz4 and zstd.
sub check_pg_config
{
	my ($regexp) = @_;
	return $regexp =~ /HAVE_LIBZ|USE_LZ4|USE_ZSTD/ ? 1 : 0;
}

# The environment a client program gets: the server, plus anything the test set in %ENV that
# libpq reads.
sub client_env
{
	my @env = (
		"PGHOST=$ENV{PGDUMP_TAP_HOST}", "PGPORT=$ENV{PGDUMP_TAP_PORT}",
		'PGUSER=postgres', "PGPASSWORD=$ENV{PGDUMP_TAP_PASSWORD}",
		'PGDATABASE=postgres', 'LC_MESSAGES=C', 'PGAPPNAME=pg_dump_tap');
	for my $k (qw(PGOPTIONS PGCLIENTENCODING PGSERVICE))
	{
		push @env, "$k=$ENV{$k}" if defined $ENV{$k};
	}
	return @env;
}

# Runs a client program in the container. Returns (exit code, stdout, stderr).
sub run_client
{
	my ($cmd, $stdin) = @_;
	my @argv = ('docker', 'exec', '-i');
	push @argv, ('-e', $_) for client_env();
	push @argv, ('-w', $ENV{PGDUMP_TAP_DIR});
	push @argv, $ENV{PGDUMP_TAP_CONTAINER}, @$cmd;
	my $err = gensym;
	my $pid = open3(my $in, my $out, $err, @argv);
	print $in $stdin if defined $stdin;
	close $in;
	# Read stderr in a child so that neither pipe fills up.
	my $errtext = '';
	my $outtext = '';
	{
		local $/;
		my $kid = open(my $errpipe, '-|') // die "fork: $!";
		if (!$kid)
		{
			my $e = <$err>;
			print $e // '';
			exit 0;
		}
		$outtext = <$out> // '';
		$errtext = <$errpipe> // '';
		close $errpipe;
	}
	waitpid($pid, 0);
	my $code = $? >> 8;
	return ($code, $outtext, $errtext);
}

sub run_command
{
	my ($cmd) = @_;
	my ($code, $out, $err) = run_client($cmd);
	chomp($out);
	chomp($err);
	return ($out, $err);
}

sub command_ok
{
	my ($cmd, $test_name) = @_;
	my ($code, $out, $err) = run_client($cmd);
	my $ok = ok($code == 0, $test_name);
	diag("command failed: @$cmd\n$err") if !$ok;
	return $ok;
}

sub command_fails
{
	my ($cmd, $test_name) = @_;
	my ($code) = run_client($cmd);
	return ok($code != 0, $test_name);
}

sub command_exit_is
{
	my ($cmd, $expected, $test_name) = @_;
	my ($code) = run_client($cmd);
	return is($code, $expected, $test_name);
}

sub command_like
{
	my ($cmd, $expected_stdout, $test_name) = @_;
	my ($code, $out, $err) = run_client($cmd);
	ok($code == 0, "$test_name: exit code 0");
	is($err, '', "$test_name: no stderr");
	like($out, $expected_stdout, "$test_name: matches");
}

sub command_fails_like
{
	my ($cmd, $expected_stderr, $test_name) = @_;
	my ($code, $out, $err) = run_client($cmd);
	ok($code != 0, "$test_name: exit code not 0");
	like($err, $expected_stderr, "$test_name: matches");
}

sub command_ok_or_fails_like
{
	my ($cmd, $expected_stdout, $expected_stderr, $test_name) = @_;
	my ($code, $out, $err) = run_client($cmd);
	if ($code == 0)
	{
		like($out, $expected_stdout, "$test_name: stdout matches");
	}
	else
	{
		like($err, $expected_stderr, "$test_name: stderr matches");
	}
}

sub command_checks_all
{
	my ($cmd, $expected_ret, $out_re, $err_re, $test_name) = @_;
	my ($code, $out, $err) = run_client($cmd);
	is($code, $expected_ret, "$test_name status (got $code vs expected $expected_ret)");
	like($out, $_, "$test_name stdout /$_/") for @$out_re;
	like($err, $_, "$test_name stderr /$_/") for @$err_re;
}

# Runs a client program; true when it succeeded.
sub run_log
{
	my ($cmd) = @_;
	my ($code, $out, $err) = run_client($cmd);
	print STDERR "# run_log failed: @$cmd\n$err" if $code != 0;
	return $code == 0;
}

sub system_log { return run_log([@_]); }

# The client programs' --help / --version / option handling (as PostgreSQL's do).
sub program_help_ok
{
	my ($cmd) = @_;
	my ($code, $out, $err) = run_client([ $cmd, '--help' ]);
	ok($code == 0, "$cmd --help exit code 0");
	isnt($out, '', "$cmd --help goes to stdout");
	is($err, '', "$cmd --help nothing to stderr");
}

sub program_version_ok
{
	my ($cmd) = @_;
	my ($code, $out, $err) = run_client([ $cmd, '--version' ]);
	ok($code == 0, "$cmd --version exit code 0");
	isnt($out, '', "$cmd --version goes to stdout");
	is($err, '', "$cmd --version nothing to stderr");
}

sub program_options_handling_ok
{
	my ($cmd) = @_;
	my ($code, $out, $err) = run_client([ $cmd, '--not-a-valid-option' ]);
	isnt($code, 0, "$cmd with invalid option nonzero exit code");
	isnt($err, '', "$cmd with invalid option prints error message");
}

1;
