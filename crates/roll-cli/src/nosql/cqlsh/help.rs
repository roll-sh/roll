//! HELP: the shell commands' help texts (the docstrings of Apache Cassandra's `cqlshmain.py`,
//! Apache-2.0, as both references print them) and the CQL topics of `helptopics.py`, which cqlsh
//! answers with a documentation URL.

use super::format::Flavor;

/// The text `HELP <topic>` prints for a cqlsh command.
pub fn shell_topic(flavor: Flavor, topic: &str) -> Option<&'static str> {
    let t = topic.to_lowercase();
    Some(match t.as_str() {
        "capture" => include_str!("help/CAPTURE.txt"),
        "clear" | "cls" => include_str!("help/CLEAR.txt"),
        "consistency" => include_str!("help/CONSISTENCY.txt"),
        "copy" => include_str!("help/COPY.txt"),
        "exit" => include_str!("help/EXIT.txt"),
        "expand" => include_str!("help/EXPAND.txt"),
        "help" => include_str!("help/HELP.txt"),
        "login" => include_str!("help/LOGIN.txt"),
        "paging" => include_str!("help/PAGING.txt"),
        "serial" => include_str!("help/SERIAL.txt"),
        "show" => include_str!("help/SHOW.txt"),
        "source" => include_str!("help/SOURCE.txt"),
        "tracing" => include_str!("help/TRACING.txt"),
        "unicode" => include_str!("help/UNICODE.txt"),
        "describe" | "desc" if flavor == Flavor::Cassandra => include_str!("help/DESCRIBE.txt"),
        "history" if flavor == Flavor::Cassandra => include_str!("help/HISTORY.txt"),
        _ => return None,
    })
}

/// `CQL3HelpTopics`: topic → anchor in the CQL documentation.
const CQL_TOPICS: &[(&str, &str)] = &[
    ("aggregates", "aggregates"),
    ("alter_keyspace", "alterKeyspaceStmt"),
    ("alter_materialized_view", "alterMVStmt"),
    ("alter_table", "alterTableStmt"),
    ("alter_type", "alterTypeStmt"),
    ("alter_user", "alterUserStmt"),
    ("apply", "batchStmt"),
    ("ascii", "constants"),
    ("batch", "batchStmt"),
    ("begin", "batchStmt"),
    ("blob", "constants"),
    ("boolean", "constants"),
    ("counter", "counters"),
    ("create_aggregate", "createAggregateStmt"),
    ("create_columnfamily", "createTableStmt"),
    ("create_function", "createFunctionStmt"),
    ("create_index", "createIndexStmt"),
    ("create_keyspace", "createKeyspaceStmt"),
    ("create_materialized_view", "createMVStmt"),
    ("create_role", "createRoleStmt"),
    ("create_table", "createTableStmt"),
    ("create_trigger", "createTriggerStmt"),
    ("create_type", "createTypeStmt"),
    ("create_user", "createUserStmt"),
    ("date", "usingdates"),
    ("delete", "deleteStmt"),
    ("drop_aggregate", "dropAggregateStmt"),
    ("drop_columnfamily", "dropTableStmt"),
    ("drop_function", "dropFunctionStmt"),
    ("drop_index", "dropIndexStmt"),
    ("drop_keyspace", "dropKeyspaceStmt"),
    ("drop_materialized_view", "dropMVStmt"),
    ("drop_role", "dropRoleStmt"),
    ("drop_table", "dropTableStmt"),
    ("drop_trigger", "dropTriggerStmt"),
    ("drop_type", "dropTypeStmt"),
    ("drop_user", "dropUserStmt"),
    ("functions", "functions"),
    ("grant", "grantRoleStmt"),
    ("insert", "insertStmt"),
    ("insert_json", "insertJson"),
    ("int", "constants"),
    ("json", "json"),
    ("keywords", "appendixA"),
    ("list_permissions", "listPermissionsStmt"),
    ("list_roles", "listRolesStmt"),
    ("list_users", "listUsersStmt"),
    ("permissions", "permissions"),
    ("revoke", "revokeRoleStmt"),
    ("select", "selectStmt"),
    ("select_json", "selectJson"),
    ("text", "constants"),
    ("time", "usingtime"),
    ("timestamp", "usingtimestamps"),
    ("truncate", "truncateStmt"),
    ("types", "types"),
    ("update", "updateStmt"),
    ("use", "useStmt"),
    ("uuid", "constants"),
];

pub fn cql_topic(topic: &str) -> Option<&'static str> {
    let t = topic.to_lowercase();
    CQL_TOPICS.iter().find(|(n, _)| *n == t).map(|(_, a)| *a)
}

/// Every HELP topic, for completion.
pub fn topics(flavor: Flavor) -> Vec<String> {
    let mut v: Vec<String> = CQL_TOPICS.iter().map(|(n, _)| n.to_uppercase()).collect();
    for t in [
        "capture", "clear", "cls", "consistency", "copy", "describe", "desc", "exit", "expand", "help", "history", "login", "paging", "serial",
        "show", "source", "tracing", "unicode",
    ] {
        if shell_topic(flavor, t).is_some() {
            v.push(t.to_uppercase());
        }
    }
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_per_flavor() {
        assert!(shell_topic(Flavor::Cassandra, "DESC").is_some());
        assert!(shell_topic(Flavor::Scylla, "describe").is_none());
        assert!(shell_topic(Flavor::Scylla, "Consistency").unwrap().contains("CONSISTENCY [cqlsh only]"));
        assert_eq!(cql_topic("SELECT"), Some("selectStmt"));
        assert_eq!(cql_topic("nope"), None);
        assert!(topics(Flavor::Cassandra).contains(&"HISTORY".to_string()));
        assert!(!topics(Flavor::Scylla).contains(&"HISTORY".to_string()));
    }
}
