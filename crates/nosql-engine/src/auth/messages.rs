//! Every auth message, per dialect. Texts were captured from the auth-enabled references
//! (`scylladb/scylla:2026.3.3` and `cassandra:5.0.9` with `PasswordAuthenticator` +
//! `CassandraAuthorizer`; cases in `tests/captured_auth.rs`) unless marked *unverified*. Kept
//! here rather than in `crate::messages` so the auth subsystem owns its texts.

use nosql_types::Dialect;

use crate::error::{Error, ErrorCode};

fn unauthorized(m: impl Into<String>) -> Error {
    Error::unauthorized(m)
}

/// `ClientState.ensureNotAnonymous`.
pub fn anonymous() -> Error {
    unauthorized("You have to be logged in and not anonymous to perform this request")
}

/// The data-permission failure (`ClientState.ensurePermissionOnResourceChain`).
pub fn no_permission(user: &str, perm: &str, resource: &str) -> Error {
    unauthorized(format!(
        "User {user} has no {perm} permission on {resource} or any of its parents"
    ))
}

/// Cassandra's role statements wrap permission failures (`AuthenticationStatement`).
pub fn insufficient_privileges(user: &str) -> Error {
    unauthorized(format!(
        "User {user} does not have sufficient privileges to perform the requested operation"
    ))
}

/// A role-statement permission failure in the dialect's wording.
pub fn role_permission(d: Dialect, user: &str, perm: &str, resource: &str) -> Error {
    match d {
        Dialect::Scylla => no_permission(user, perm, resource),
        Dialect::Cassandra => insufficient_privileges(user),
    }
}

/// ScyllaDB's failure on the tables the external index services read.
pub fn none_of_indexing(user: &str, resource: &str) -> Error {
    unauthorized(format!(
        "User {user} has none of the permissions (SELECT, TEXT_SEARCH_INDEXING, VECTOR_SEARCH_INDEXING) on {resource} or any of its parents"
    ))
}

/// ScyllaDB: writes to system tables by ordinary users.
pub fn system_select_only(keyspace: &str) -> Error {
    unauthorized(format!(
        "{keyspace} can be granted only SELECT or DESCRIBE permissions to a non-superuser."
    ))
}

/// Cassandra's `preventSystemKSSchemaModification` for replicated system keyspaces (and
/// ScyllaDB's for `DROP KEYSPACE` / types there).
pub fn cannot(perm: &str, resource: &str) -> Error {
    unauthorized(format!("Cannot {perm} {resource}"))
}

/// ScyllaDB: schema changes on its protected auth tables.
pub fn protected(resource: &str) -> Error {
    unauthorized(format!("{resource} is protected"))
}

pub fn not_user_modifiable(keyspace: &str) -> Error {
    unauthorized(format!("{keyspace} keyspace is not user-modifiable."))
}

/// `You do not have access to this datacenter (dc)` (Cassandra network authorizer).
pub fn no_dc_access(dc: &str) -> Error {
    unauthorized(format!("You do not have access to this datacenter ({dc})"))
}

pub fn role_exists(d: Dialect, role: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Role {role} already exists."),
        Dialect::Cassandra => format!("{role} already exists"),
    })
}

/// A missing role named by DROP / GRANT role statements.
pub fn role_missing(d: Dialect, role: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Role {role} doesn't exist."),
        Dialect::Cassandra => format!("{role} doesn't exist"),
    })
}

/// A missing grantee of GRANT/REVOKE permissions, or ALTER of a missing role.
pub fn role_missing_prefixed(d: Dialect, role: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Role {role} doesn't exist."),
        Dialect::Cassandra => format!("Role {role} doesn't exist"),
    })
}

/// A missing role named by LIST statements (Cassandra prints the resource).
pub fn list_role_missing(d: Dialect, role: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Role {role} doesn't exist."),
        Dialect::Cassandra => format!("<role {role}> doesn't exist"),
    })
}

/// GRANT/REVOKE on a resource that doesn't exist.
pub fn resource_missing(d: Dialect, resource: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("{resource} doesn't exist."),
        Dialect::Cassandra => format!("Resource {resource} doesn't exist"),
    })
}

/// LIST PERMISSIONS on a resource that doesn't exist.
pub fn list_resource_missing(d: Dialect, resource: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("{resource} doesn't exist."),
        Dialect::Cassandra => format!("{resource} doesn't exist"),
    })
}

pub fn drop_primary_role(d: Dialect) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => "Cannot DROP primary role for current login.",
        Dialect::Cassandra => "Cannot DROP primary role for current login",
    })
}

pub fn create_superuser_only(d: Dialect) -> Error {
    unauthorized(match d {
        Dialect::Scylla => "Only superusers can create a role with superuser status.",
        Dialect::Cassandra => "Only superusers can create a role with superuser status",
    })
}

pub fn drop_superuser_only(d: Dialect) -> Error {
    unauthorized(match d {
        Dialect::Scylla => "Only superusers can drop a superuser role.",
        Dialect::Cassandra => "Only superusers can drop a role with superuser status",
    })
}

pub fn alter_superuser_only(d: Dialect) -> Error {
    unauthorized(match d {
        Dialect::Scylla => "Only superusers are allowed to alter superuser status.",
        Dialect::Cassandra => "Only superusers are allowed to alter superuser status",
    })
}

pub fn alter_own_superuser(d: Dialect) -> Error {
    unauthorized(match d {
        Dialect::Scylla => {
            "You aren't allowed to alter your own superuser status or that of a role granted to you."
        }
        Dialect::Cassandra => {
            "You aren't allowed to alter your own superuser status or that of a role granted to you"
        }
    })
}

/// Cassandra: a non-superuser altering an option of its own role it may not change.
pub fn alter_own_option(option: &str) -> Error {
    unauthorized(format!("You aren't allowed to alter {option}"))
}

/// ScyllaDB: a non-superuser altering `OPTIONS` of its own role.
pub fn scylla_alter_own_options() -> Error {
    unauthorized("You aren't allowed to alter the OPTIONS option.")
}

pub fn alter_dcs_superuser_only() -> Error {
    unauthorized("Only superusers are allowed to alter access to datacenters.")
}

pub fn alter_cidrs_superuser_only() -> Error {
    unauthorized("Only superusers are allowed to alter access from CIDR groups.")
}

/// Cassandra: `RoleOptions.validate` with an option the role manager doesn't support.
pub fn unsupported_option(option: &str) -> Error {
    Error::invalid(format!(
        "org.apache.cassandra.auth.CassandraRoleManager doesn't support {option}"
    ))
}

pub fn scylla_options_unsupported() -> Error {
    Error::invalid("The OPTIONS option is not supported.")
}

pub fn scylla_password_and_hashed() -> Error {
    Error::syntax("Only one of the options: PASSWORD, HASHED PASSWORD can be provided")
}

pub fn scylla_hashed_superuser_only() -> Error {
    unauthorized("Only superusers can create a role with a hashed password.")
}

/// ScyllaDB answers `ALTER ROLE … WITH HASHED PASSWORD` with this server error.
pub fn scylla_alter_hashed() -> Error {
    Error::server("std::get: wrong index for variant")
}

pub fn invalid_bcrypt() -> Error {
    Error::invalid("Invalid hashed password value. Please use jBcrypt.")
}

pub fn empty_role_name(d: Dialect) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => {
            "Exception while binding column role: marshaling error: Validation failed - non-UTF8 character in a UTF8 string, at byte offset 0"
        }
        Dialect::Cassandra => "Role name can't be an empty string",
    })
}

pub fn alter_empty() -> Error {
    Error::invalid("ALTER [ROLE|USER] can't be empty")
}

/// Cassandra rate-limits password changes (`ROLE_PASSWORD_UPDATE_MIN_INTERVAL_MS`).
pub fn password_rate_limited(role: &str, interval_ms: u64) -> Error {
    Error::new(
        ErrorCode::Overloaded,
        format!("Password for role {role} can only be changed every {interval_ms}ms."),
    )
}

pub fn invalid_datacenters(dcs: &[String]) -> Error {
    Error::invalid(format!(
        "Invalid value(s) for DATACENTERS '[{}]',All values must be valid datacenters",
        dcs.join(", ")
    ))
}

pub fn dcs_not_supported() -> Error {
    Error::invalid("ACCESS TO DATACENTERS operations not supported by AllowAllNetworkAuthorizer")
}

pub fn invalid_cidr_groups(groups: &[String]) -> Error {
    Error::invalid(format!(
        "Invalid CIDR group(s): [{}]. Available CIDR Groups are: []",
        groups.join(", ")
    ))
}

/// GRANT role: `grantee` already holds `role` (directly or not).
pub fn already_member(d: Dialect, grantee: &str, role: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("{grantee} already includes role {role}."),
        Dialect::Cassandra => format!("{grantee} is a member of {role}"),
    })
}

pub fn not_member(d: Dialect, revokee: &str, role: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => {
            format!("{revokee} was not granted role {role}, so it cannot be revoked.")
        }
        Dialect::Cassandra => format!("{revokee} is not a member of {role}"),
    })
}

pub fn view_roles_of(d: Dialect, role: &str) -> Error {
    unauthorized(match d {
        Dialect::Scylla => {
            format!("You are not authorized to view the roles granted to role '{role}'.")
        }
        Dialect::Cassandra => format!("You are not authorized to view roles granted to {role} "),
    })
}

/// LIST PERMISSIONS of someone else (`everyone` for no role).
pub fn view_permissions_of(who: &str) -> Error {
    unauthorized(format!(
        "You are not authorized to view {who}'s permissions"
    ))
}

pub fn system_keyspace_grant() -> Error {
    unauthorized(
        "Granting permissions on system keyspaces is strictly limited, this operation is not permitted",
    )
}

pub fn builtin_function_permissions() -> Error {
    Error::invalid("Altering permissions on builtin functions is not supported")
}

/// Cassandra fails an assertion for GRANT on a function that doesn't exist.
pub fn cassandra_missing_function(resource: &str) -> Error {
    Error::server(format!(
        "java.lang.AssertionError: Unable to find function object for resource {resource}"
    ))
}

/// `AllowAllAuthorizer` operations (`UnsupportedOperationException`).
pub fn allow_all_unsupported(op: &str) -> Error {
    Error::server(format!(
        "java.lang.UnsupportedOperationException: {op} operation is not supported by AllowAllAuthorizer"
    ))
}

pub fn allow_all_list() -> Error {
    Error::invalid("LIST PERMISSIONS operation is not supported by AllowAllAuthorizer")
}

pub fn already_granted(role: &str, perms: &str, resource: &str) -> String {
    format!("Role '{role}' was already granted {perms} on {resource}")
}

pub fn not_granted(role: &str, perms: &str, resource: &str) -> String {
    format!("Role '{role}' was not granted {perms} on {resource}")
}

// Identities (Cassandra 5).

pub fn identity_superuser_only_add() -> Error {
    unauthorized("Only superusers can bind identities to a role with superuser status")
}

pub fn identity_superuser_only_drop() -> Error {
    unauthorized("Only superusers can remove identity bindings from a role with superuser status")
}

pub fn identity_role_missing(role: &str) -> Error {
    Error::invalid(format!(
        "Can not add identity for non-existent role '{role}'"
    ))
}

pub fn identity_exists(identity: &str) -> Error {
    Error::invalid(format!("{identity} already exists"))
}

pub fn identity_missing(identity: &str) -> Error {
    Error::invalid(format!("identity '{identity}' doesn't exist"))
}

pub fn create_trigger_superuser() -> Error {
    unauthorized("Only superusers are allowed to perform CREATE TRIGGER queries")
}

pub fn drop_trigger_superuser() -> Error {
    unauthorized("Only superusers are allowed to perfrom DROP TRIGGER queries")
}

// Login.

/// A failed password check.
pub fn bad_credentials(d: Dialect, user: &str) -> Error {
    Error::new(
        ErrorCode::BadCredentials,
        match d {
            Dialect::Scylla => "Username and/or password are incorrect".to_string(),
            Dialect::Cassandra => {
                format!("Provided username {user} and/or password are incorrect")
            }
        },
    )
}

pub fn bad_credentials_text(m: &str) -> Error {
    Error::new(ErrorCode::BadCredentials, m)
}

pub fn not_permitted_to_login(user: &str) -> Error {
    Error::new(
        ErrorCode::BadCredentials,
        format!("{user} is not permitted to log in"),
    )
}

// ScyllaDB service levels.

pub fn sl_exists(name: &str) -> Error {
    Error::invalid(format!("The service level '{name}' already exists."))
}

pub fn sl_missing_alter(name: &str) -> Error {
    Error::invalid(format!("The service level '{name}' doesn't exist."))
}

pub fn sl_missing(name: &str) -> Error {
    Error::invalid(format!("Service Level {name} doesn't exists."))
}

pub fn sl_default_create() -> Error {
    Error::invalid("The default service level, default, already exists and cannot be created")
}

pub fn sl_default_alter() -> Error {
    Error::invalid("The default service level, default, cannot be altered")
}

pub fn sl_default_drop() -> Error {
    Error::invalid("The default service level, default, cannot be dropped")
}

pub fn sl_default_attach() -> Error {
    Error::invalid(
        "The default service level, default, cannot be attached to a role. If you want to detach an attached service level, use the DETACH SERVICE LEVEL statement",
    )
}

pub fn sl_none_assigned(role: &str) -> Error {
    Error::invalid(format!(
        "Role {role} doesn't have assigned any service level"
    ))
}

pub fn sl_unknown_property(name: &str) -> Error {
    Error::syntax(format!("Unknown property '{name}'"))
}

pub fn sl_shares_range(given: &str) -> Error {
    Error::syntax(format!(
        "'SHARES' can only take values of 1-1000 (given {given})"
    ))
}

pub fn sl_invalid_integer(value: &str) -> Error {
    Error::syntax(format!("Invalid integer value {value} for 'shares'"))
}

pub fn sl_invalid_duration(statement: &str, value: &str) -> Error {
    Error::syntax(format!(
        "Failed parsing statement: [{statement}] reason: marshaling error: Unable to convert '{value}' to a duration"
    ))
}

pub fn sl_value_not_string(name: &str) -> Error {
    Error::syntax(format!(
        "Invalid value for property '{name}'. It should be a string"
    ))
}

pub fn sl_timeout_negative() -> Error {
    Error::invalid("Timeout values must be nonnegative")
}

pub fn sl_timeout_days() -> Error {
    Error::invalid("Timeout values cannot be expressed in days/months")
}

pub fn sl_timeout_granularity() -> Error {
    Error::invalid("Timeout values must be expressed in millisecond granularity")
}

pub fn sl_workload(value: &str) -> Error {
    Error::invalid(format!("Invalid workload type: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_texts() {
        assert_eq!(
            role_exists(Dialect::Scylla, "a").message,
            "Role a already exists."
        );
        assert_eq!(
            role_exists(Dialect::Cassandra, "a").message,
            "a already exists"
        );
        assert_eq!(
            already_member(Dialect::Cassandra, "a", "b").message,
            "a is a member of b"
        );
        assert_eq!(
            view_roles_of(Dialect::Cassandra, "b").message,
            "You are not authorized to view roles granted to b "
        );
        assert_eq!(
            bad_credentials(Dialect::Cassandra, "u").code,
            ErrorCode::BadCredentials
        );
        assert_eq!(password_rate_limited("r", 5000).code, ErrorCode::Overloaded);
        assert_eq!(scylla_password_and_hashed().code, ErrorCode::Syntax);
        assert_eq!(
            invalid_datacenters(&["nope".into()]).message,
            "Invalid value(s) for DATACENTERS '[nope]',All values must be valid datacenters"
        );
    }
}
