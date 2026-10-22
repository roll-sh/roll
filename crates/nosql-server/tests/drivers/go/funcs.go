package main

import "github.com/gocql/gocql"

func createFunctions(s *gocql.Session) error {
	stmts := []string{
		"CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x * 2;'",
		"CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE java AS 'return (acc == null ? 0 : acc) + (x == null ? 0 : x);'",
	}
	if dialect == "scylla" {
		stmts = []string{
			"CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x * 2'",
			"CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return (acc or 0) + (x or 0)'",
		}
	}
	stmts = append(stmts, "CREATE AGGREGATE total(int) SFUNC plus STYPE int INITCOND 0")
	for _, q := range stmts {
		if err := s.Query(q).Exec(); err != nil {
			return err
		}
	}
	return nil
}
