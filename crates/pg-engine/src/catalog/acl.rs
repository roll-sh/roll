//! Privileges: aclitem arrays as GRANT / REVOKE maintain them, role membership, and the
//! privilege masks permission checks use (Postgres's acl.c / aclchk.c).

use super::{BOOTSTRAP_SUPERUSER_OID, Catalog, PG_DATABASE_OWNER_OID};
use crate::error::{Error, Result, code};
use serde::{Deserialize, Serialize};

pub const INSERT: u32 = 1 << 0;
pub const SELECT: u32 = 1 << 1;
pub const UPDATE: u32 = 1 << 2;
pub const DELETE: u32 = 1 << 3;
pub const TRUNCATE: u32 = 1 << 4;
pub const REFERENCES: u32 = 1 << 5;
pub const TRIGGER: u32 = 1 << 6;
pub const EXECUTE: u32 = 1 << 7;
pub const USAGE: u32 = 1 << 8;
pub const CREATE: u32 = 1 << 9;
pub const TEMPORARY: u32 = 1 << 10;
pub const CONNECT: u32 = 1 << 11;
pub const SET: u32 = 1 << 12;
pub const ALTER_SYSTEM: u32 = 1 << 13;
pub const MAINTAIN: u32 = 1 << 14;

/// What column privileges can be.
pub const COLUMN_PRIVS: u32 = SELECT | INSERT | UPDATE | REFERENCES;

/// Privilege letters, in aclitem output order.
const LETTERS: &[(u32, char)] = &[
    (INSERT, 'a'),
    (SELECT, 'r'),
    (UPDATE, 'w'),
    (DELETE, 'd'),
    (TRUNCATE, 'D'),
    (REFERENCES, 'x'),
    (TRIGGER, 't'),
    (EXECUTE, 'X'),
    (USAGE, 'U'),
    (CREATE, 'C'),
    (TEMPORARY, 'T'),
    (CONNECT, 'c'),
    (SET, 's'),
    (ALTER_SYSTEM, 'A'),
    (MAINTAIN, 'm'),
];

/// Privilege names (aclexplode, information_schema, error messages).
pub const NAMES: &[(u32, &str)] = &[
    (INSERT, "INSERT"),
    (SELECT, "SELECT"),
    (UPDATE, "UPDATE"),
    (DELETE, "DELETE"),
    (TRUNCATE, "TRUNCATE"),
    (REFERENCES, "REFERENCES"),
    (TRIGGER, "TRIGGER"),
    (EXECUTE, "EXECUTE"),
    (USAGE, "USAGE"),
    (CREATE, "CREATE"),
    (TEMPORARY, "TEMPORARY"),
    (CONNECT, "CONNECT"),
    (SET, "SET"),
    (ALTER_SYSTEM, "ALTER SYSTEM"),
    (MAINTAIN, "MAINTAIN"),
];

/// A privilege by its SQL name (`GRANT select ...`, has_table_privilege's mode words).
pub fn privilege_by_name(name: &str) -> Option<u32> {
    Some(match name {
        "select" => SELECT,
        "insert" => INSERT,
        "update" => UPDATE,
        "delete" => DELETE,
        "truncate" => TRUNCATE,
        "references" => REFERENCES,
        "trigger" => TRIGGER,
        "execute" => EXECUTE,
        "usage" => USAGE,
        "create" => CREATE,
        "temporary" | "temp" => TEMPORARY,
        "connect" => CONNECT,
        "set" => SET,
        "alter system" => ALTER_SYSTEM,
        "maintain" => MAINTAIN,
        _ => return None,
    })
}

pub fn privilege_name(bit: u32) -> &'static str {
    NAMES.iter().find(|(b, _)| *b == bit).map_or("?", |n| n.1)
}

/// One aclitem: `grantee=privs/grantor`, grantee 0 being PUBLIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclItem {
    pub grantee: u32,
    pub grantor: u32,
    pub privs: u32,
    /// Privileges held WITH GRANT OPTION (a subset of `privs`).
    pub goptions: u32,
}

pub type Acl = Vec<AclItem>;

/// The kinds of object that carry an ACL (acldefault's letters).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjKind {
    Table,
    Sequence,
    Database,
    Function,
    Language,
    LargeObject,
    Schema,
    Tablespace,
    Type,
    ForeignDataWrapper,
    ForeignServer,
    Parameter,
}

impl ObjKind {
    /// Every privilege the kind supports.
    pub fn all(self) -> u32 {
        match self {
            ObjKind::Table => {
                INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER | MAINTAIN
            }
            ObjKind::Sequence => USAGE | SELECT | UPDATE,
            ObjKind::Database => CREATE | TEMPORARY | CONNECT,
            ObjKind::Function => EXECUTE,
            ObjKind::Language
            | ObjKind::Type
            | ObjKind::ForeignDataWrapper
            | ObjKind::ForeignServer => USAGE,
            ObjKind::LargeObject => SELECT | UPDATE,
            ObjKind::Schema => USAGE | CREATE,
            ObjKind::Tablespace => CREATE,
            ObjKind::Parameter => SET | ALTER_SYSTEM,
        }
    }

    /// What PUBLIC holds by default.
    pub fn world_default(self) -> u32 {
        match self {
            ObjKind::Database => TEMPORARY | CONNECT,
            ObjKind::Function => EXECUTE,
            ObjKind::Language | ObjKind::Type => USAGE,
            _ => 0,
        }
    }

    pub fn letter(self) -> char {
        match self {
            ObjKind::Table => 'r',
            ObjKind::Sequence => 's',
            ObjKind::Database => 'd',
            ObjKind::Function => 'f',
            ObjKind::Language => 'l',
            ObjKind::LargeObject => 'L',
            ObjKind::Schema => 'n',
            ObjKind::Tablespace => 't',
            ObjKind::Type => 'T',
            ObjKind::ForeignDataWrapper => 'F',
            ObjKind::ForeignServer => 'S',
            ObjKind::Parameter => 'p',
        }
    }

    pub fn from_letter(c: char) -> Option<ObjKind> {
        Some(match c {
            'r' => ObjKind::Table,
            's' => ObjKind::Sequence,
            'd' => ObjKind::Database,
            'f' => ObjKind::Function,
            'l' => ObjKind::Language,
            'L' => ObjKind::LargeObject,
            'n' => ObjKind::Schema,
            't' => ObjKind::Tablespace,
            'T' => ObjKind::Type,
            'F' => ObjKind::ForeignDataWrapper,
            'S' => ObjKind::ForeignServer,
            'p' => ObjKind::Parameter,
            _ => return None,
        })
    }

    /// The word GRANT errors use ("invalid privilege type USAGE for table").
    pub fn word(self) -> &'static str {
        match self {
            ObjKind::Table => "relation",
            ObjKind::Sequence => "sequence",
            ObjKind::Database => "database",
            ObjKind::Function => "function",
            ObjKind::Language => "language",
            ObjKind::LargeObject => "large object",
            ObjKind::Schema => "schema",
            ObjKind::Tablespace => "tablespace",
            ObjKind::Type => "type",
            ObjKind::ForeignDataWrapper => "foreign-data wrapper",
            ObjKind::ForeignServer => "foreign server",
            ObjKind::Parameter => "parameter",
        }
    }

    /// pg_class / pg_namespace / … oid that `acls` keys the kind under.
    pub fn class_oid(self) -> u32 {
        match self {
            ObjKind::Table | ObjKind::Sequence => 1259,
            ObjKind::Database => 1262,
            ObjKind::Function => 1255,
            ObjKind::Language => 2612,
            ObjKind::LargeObject => 2613,
            ObjKind::Schema => 2615,
            ObjKind::Tablespace => 1213,
            ObjKind::Type => 1247,
            ObjKind::ForeignDataWrapper => 2328,
            ObjKind::ForeignServer => 1417,
            ObjKind::Parameter => 6243,
        }
    }
}

/// Built-in functions whose EXECUTE isn't PUBLIC's (system_functions.sql revokes it): the
/// predefined role also granted it, if any.
const RESTRICTED_PROCS: &[(u32, u32)] = &[
    (764, 0),
    (765, 0),
    (767, 0),
    (2172, 0),
    (2274, 0),
    (2282, 3375),
    (2307, 0),
    (2621, 0),
    (2622, 0),
    (2623, 0),
    (2624, 0),
    (2625, 0),
    (2739, 0),
    (2848, 0),
    (3071, 0),
    (3072, 0),
    (3098, 0),
    (3293, 0),
    (3295, 0),
    (3297, 0),
    (3307, 0),
    (3329, 0),
    (3353, 3373),
    (3354, 3373),
    (3400, 0),
    (3401, 0),
    (3436, 0),
    (3775, 0),
    (3776, 0),
    (3777, 0),
    (3800, 3373),
    (3801, 3373),
    (3826, 0),
    (3827, 0),
    (3828, 0),
    (4100, 3375),
    (4543, 0),
    (5029, 3373),
    (5030, 3373),
    (5031, 3373),
    (5052, 3375),
    (6003, 0),
    (6004, 0),
    (6005, 0),
    (6006, 0),
    (6007, 0),
    (6008, 0),
    (6009, 0),
    (6010, 0),
    (6011, 0),
    (6012, 0),
    (6013, 0),
    (6014, 0),
    (6170, 0),
    (6208, 0),
    (6209, 0),
    (6230, 0),
    (6232, 0),
    (6250, 0),
    (6270, 3373),
    (6271, 3373),
    (6272, 3373),
    (6305, 0),
    (6387, 0),
    (6399, 3375),
    (6400, 3373),
];

/// The initial ACL of a built-in function whose EXECUTE was revoked from PUBLIC.
pub fn builtin_proc_acl(oid: u32) -> Option<Acl> {
    let (_, extra) = RESTRICTED_PROCS.iter().find(|p| p.0 == oid)?;
    let owner = BOOTSTRAP_SUPERUSER_OID;
    let mut acl = vec![AclItem {
        grantee: owner,
        grantor: owner,
        privs: EXECUTE,
        goptions: 0,
    }];
    if *extra != 0 {
        acl.push(AclItem {
            grantee: *extra,
            grantor: owner,
            privs: EXECUTE,
            goptions: 0,
        });
    }
    Some(acl)
}

/// acldefault(kind, owner): PUBLIC's default first, then the owner's.
pub fn default_acl(kind: ObjKind, owner: u32) -> Acl {
    let mut acl = Vec::new();
    if kind.world_default() != 0 {
        acl.push(AclItem {
            grantee: 0,
            grantor: owner,
            privs: kind.world_default(),
            goptions: 0,
        });
    }
    acl.push(AclItem {
        grantee: owner,
        grantor: owner,
        privs: kind.all(),
        goptions: 0,
    });
    acl
}

/// The ACL an object has until a GRANT or REVOKE stores one: a built-in function's
/// initial ACL, or acldefault.
pub fn initial_acl(kind: ObjKind, oid: u32, owner: u32) -> Acl {
    match kind {
        ObjKind::Function => builtin_proc_acl(oid).unwrap_or_else(|| default_acl(kind, owner)),
        _ => default_acl(kind, owner),
    }
}

/// The object's ACL key in `Catalog::acls`: (class oid, object oid, column number or 0).
pub type AclKey = (u32, u32, i32);

/// The stored ACL of an object, as a serialisable list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjAcl {
    pub class: u32,
    pub oid: u32,
    #[serde(default)]
    pub sub: i32,
    pub acl: Acl,
}

/// ALTER DEFAULT PRIVILEGES: the ACL objects `role` creates (in `schema`, 0 for any) start with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DefaultAcl {
    pub oid: u32,
    pub role: u32,
    pub schema: u32,
    /// 'r' relations, 'S' sequences, 'f' functions, 'T' types, 'n' schemas.
    pub objtype: char,
    pub acl: Acl,
}

/// A role membership (pg_auth_members).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthMember {
    pub oid: u32,
    pub role: u32,
    pub member: u32,
    pub grantor: u32,
    pub admin: bool,
    pub inherit: bool,
    pub set: bool,
}

/// An identifier as aclitem output shows it: bare when it is all lowercase letters, digits
/// and underscores, double-quoted otherwise.
fn putid(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii())
    {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// aclitemout.
pub fn format_item(item: &AclItem, name: &dyn Fn(u32) -> Option<String>) -> String {
    let who = |oid: u32| {
        name(oid)
            .map(|n| putid(&n))
            .unwrap_or_else(|| oid.to_string())
    };
    let mut s = if item.grantee == 0 {
        String::new()
    } else {
        who(item.grantee)
    };
    s.push('=');
    for (bit, c) in LETTERS {
        if item.privs & bit != 0 {
            s.push(*c);
            if item.goptions & bit != 0 {
                s.push('*');
            }
        }
    }
    s.push('/');
    s.push_str(&who(item.grantor));
    s
}

/// `{a=r/b,...}` as a text array literal of aclitems.
pub fn format_acl(acl: &Acl, name: &dyn Fn(u32) -> Option<String>) -> Vec<String> {
    acl.iter().map(|i| format_item(i, name)).collect()
}

/// aclitemin. Returns the item and whether the grantor defaulted (Postgres warns then).
pub fn parse_item(s: &str, lookup: &dyn Fn(&str) -> Option<u32>) -> Result<(AclItem, bool)> {
    let bad = |m: String| Error::new(code::INVALID_TEXT_REPRESENTATION, m);
    let mut chars = s.trim_start().chars().peekable();
    let read_name = |chars: &mut std::iter::Peekable<std::str::Chars>| -> String {
        let mut out = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                chars.next();
                if quoted && chars.peek() == Some(&'"') {
                    chars.next();
                    out.push('"');
                    continue;
                }
                quoted = !quoted;
                continue;
            }
            if !quoted && !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-') {
                break;
            }
            chars.next();
            out.push(c);
        }
        out
    };
    let grantee_name = read_name(&mut chars);
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
    if chars.next() != Some('=') {
        return Err(bad(if grantee_name.is_empty() {
            "unrecognized key word: \"\"".into()
        } else {
            "missing \"=\" sign".into()
        })
        .hint("ACL key word must be \"group\" or \"user\"."));
    }
    let mut privs = 0;
    let mut goptions = 0;
    while let Some(&c) = chars.peek() {
        if c == '/' || c.is_whitespace() {
            break;
        }
        chars.next();
        let Some((bit, _)) = LETTERS.iter().find(|(_, l)| *l == c) else {
            return Err(bad(format!(
                "invalid mode character: must be one of \"{}\"",
                LETTERS.iter().map(|l| l.1).collect::<String>()
            )));
        };
        privs |= bit;
        if chars.peek() == Some(&'*') {
            chars.next();
            goptions |= bit;
        }
    }
    let role = |n: &str| {
        lookup(n).ok_or_else(|| {
            Error::new(
                code::UNDEFINED_OBJECT,
                format!("role \"{n}\" does not exist"),
            )
        })
    };
    let grantee = if grantee_name.is_empty() {
        0
    } else {
        role(&grantee_name)?
    };
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
    let (grantor, defaulted) = if chars.peek() == Some(&'/') {
        chars.next();
        let n = read_name(&mut chars);
        if n.is_empty() {
            return Err(bad("a name must follow the \"/\" sign".into()));
        }
        (role(&n)?, false)
    } else {
        (BOOTSTRAP_SUPERUSER_OID, true)
    };
    if chars.any(|c| !c.is_whitespace()) {
        return Err(bad(
            "extra garbage at the end of the ACL specification".into()
        ));
    }
    Ok((
        AclItem {
            grantee,
            grantor,
            privs,
            goptions,
        },
        defaulted,
    ))
}

impl Catalog {
    /// The stored ACL of an object (`None`: the default).
    pub fn acl(&self, kind: ObjKind, oid: u32, sub: i32) -> Option<&Acl> {
        if let Some(m) = self.acl_lookup() {
            return m
                .get(&(kind.class_oid(), oid, sub))
                .map(|&i| &self.acls[i].acl);
        }
        self.acls
            .iter()
            .find(|a| a.class == kind.class_oid() && a.oid == oid && a.sub == sub)
            .map(|a| &a.acl)
    }

    /// The ACL in effect: the stored one or the default for `owner`.
    pub fn effective_acl(&self, kind: ObjKind, oid: u32, owner: u32) -> Acl {
        self.acl(kind, oid, 0)
            .cloned()
            .unwrap_or_else(|| initial_acl(kind, oid, owner))
    }

    pub fn set_acl(&mut self, kind: ObjKind, oid: u32, sub: i32, acl: Option<Acl>) {
        let class = kind.class_oid();
        // A relation's relacl is in its pg_class row.
        if sub == 0 && self.class_order.contains_key(&oid) {
            self.class_moved(oid);
        }
        self.acls
            .retain(|a| !(a.class == class && a.oid == oid && a.sub == sub));
        if let Some(acl) = acl {
            self.acls.push(ObjAcl {
                class,
                oid,
                sub,
                acl,
            });
        }
    }

    /// Forget the ACLs of an object (and its columns) that no longer exists.
    pub fn drop_acls(&mut self, kind: ObjKind, oid: u32) {
        let class = kind.class_oid();
        self.acls.retain(|a| !(a.class == class && a.oid == oid));
    }

    pub fn is_superuser(&self, role: u32) -> bool {
        self.roles.get(&role).is_some_and(|r| r.superuser)
    }

    /// Roles `role` is a member of, directly or not (itself included). With `inherit_only`,
    /// only through memberships that pass privileges on (role INHERIT and grant INHERIT).
    pub fn roles_of(&self, role: u32, inherit_only: bool, db_owner: u32) -> Vec<u32> {
        let mut out = vec![role];
        let mut i = 0;
        while i < out.len() {
            let r = out[i];
            i += 1;
            // In the order of the (member, role) index, as roles_is_member_of scans it.
            let mut direct: Vec<u32> = self
                .members
                .iter()
                .filter(|m| m.member == r && (!inherit_only || m.inherit))
                .map(|m| m.role)
                .collect();
            direct.sort_unstable();
            for role in direct {
                if !out.contains(&role) {
                    out.push(role);
                }
            }
            // The database owner is implicitly a member of pg_database_owner.
            if r == db_owner && !out.contains(&PG_DATABASE_OWNER_OID) {
                out.push(PG_DATABASE_OWNER_OID);
            }
        }
        out
    }

    /// has_privs_of_role: superusers have every role's privileges.
    pub fn has_privs_of_role(&self, member: u32, role: u32, db_owner: u32) -> bool {
        member == role
            || self.is_superuser(member)
            || self.roles_of(member, true, db_owner).contains(&role)
    }

    /// is_member_of_role (ignoring INHERIT): what SET ROLE and ownership checks use.
    pub fn is_member_of_role(&self, member: u32, role: u32, db_owner: u32) -> bool {
        member == role
            || self.is_superuser(member)
            || self.roles_of(member, false, db_owner).contains(&role)
    }

    /// Whether `member` may SET ROLE to `role` (a chain of SET-option memberships).
    pub fn can_set_role(&self, member: u32, role: u32) -> bool {
        if member == role || self.is_superuser(member) {
            return true;
        }
        let mut out = vec![member];
        let mut i = 0;
        while i < out.len() {
            let r = out[i];
            i += 1;
            for m in self.members.iter().filter(|m| m.member == r && m.set) {
                if !out.contains(&m.role) {
                    out.push(m.role);
                }
            }
        }
        out.contains(&role)
    }

    /// aclmask: which of `mask` (privileges and their grant options, `goptions` bits shifted
    /// by 16) `role` holds through `acl`, owner rights and inherited memberships.
    pub fn aclmask(&self, acl: &Acl, role: u32, owner: u32, db_owner: u32) -> (u32, u32) {
        if self.is_superuser(role) {
            return (u32::MAX, u32::MAX);
        }
        let roles = self.roles_of(role, true, db_owner);
        let mut privs = 0;
        // Owners can always grant whatever they hold (and get back what they revoke).
        let mut goptions = if roles.contains(&owner) { u32::MAX } else { 0 };
        for item in acl {
            if item.grantee == 0 || roles.contains(&item.grantee) {
                privs |= item.privs;
                goptions |= item.goptions;
            }
        }
        (privs, goptions)
    }

    /// What `role` holds directly (not through memberships) — select_best_grantor's first try.
    fn direct_goptions(acl: &Acl, role: u32) -> u32 {
        acl.iter()
            .filter(|i| i.grantee == role || i.grantee == 0)
            .fold(0, |m, i| m | i.goptions)
    }

    /// select_best_grantor: who a GRANT / REVOKE by `role` acts as, and which grant options
    /// that grantor has among `wanted`.
    pub fn best_grantor(
        &self,
        role: u32,
        wanted: u32,
        acl: &Acl,
        owner: u32,
        db_owner: u32,
    ) -> (u32, u32) {
        if role == owner || self.is_superuser(role) {
            return (owner, wanted);
        }
        let mine = Self::direct_goptions(acl, role) & wanted;
        if mine == wanted {
            return (role, mine);
        }
        let mut best = (role, mine);
        for r in self.roles_of(role, true, db_owner) {
            if r == owner {
                return (owner, wanted);
            }
            let g = Self::direct_goptions(acl, r) & wanted;
            if g == wanted {
                return (r, g);
            }
            if g.count_ones() > best.1.count_ones() {
                best = (r, g);
            }
        }
        best
    }
}

/// aclupdate: add or remove `privs` (and/or their grant options) for (grantee, grantor), then
/// revoke what depended on grant options that went away.
pub fn acl_update(
    acl: &mut Acl,
    grantee: u32,
    grantor: u32,
    privs: u32,
    goptions: u32,
    add: bool,
    owner: u32,
    cascade: bool,
) -> Result<()> {
    let pos = match acl
        .iter()
        .position(|i| i.grantee == grantee && i.grantor == grantor)
    {
        Some(p) => p,
        None => {
            if !add {
                return Ok(());
            }
            acl.push(AclItem {
                grantee,
                grantor,
                privs: 0,
                goptions: 0,
            });
            acl.len() - 1
        }
    };
    let old_goptions = acl[pos].goptions;
    if add {
        acl[pos].privs |= privs;
        acl[pos].goptions |= goptions;
    } else {
        acl[pos].privs &= !privs;
        acl[pos].goptions &= !goptions;
        acl[pos].goptions &= acl[pos].privs;
    }
    let new_goptions = acl[pos].goptions;
    if acl[pos].privs == 0 && acl[pos].goptions == 0 {
        acl.remove(pos);
    }
    let lost = old_goptions & !new_goptions;
    if lost != 0 {
        recursive_revoke(acl, grantee, lost, owner, cascade)?;
    }
    Ok(())
}

/// Revoke what `grantee` granted others with grant options it no longer holds.
fn recursive_revoke(
    acl: &mut Acl,
    grantee: u32,
    mut lost: u32,
    owner: u32,
    cascade: bool,
) -> Result<()> {
    if grantee == owner {
        return Ok(());
    }
    // Still held through another grantor (or PUBLIC)?
    let still: u32 = acl
        .iter()
        .filter(|i| i.grantee == grantee || i.grantee == 0)
        .fold(0, |m, i| m | i.goptions);
    lost &= !still;
    if lost == 0 {
        return Ok(());
    }
    let dependents: Vec<AclItem> = acl
        .iter()
        .filter(|i| i.grantor == grantee && i.privs & lost != 0)
        .copied()
        .collect();
    for d in dependents {
        if !cascade {
            return Err(Error::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                "dependent privileges exist",
            )
            .hint("Use CASCADE to revoke them too."));
        }
        acl_update(acl, d.grantee, grantee, lost, lost, false, owner, true)?;
    }
    Ok(())
}

pub const PG_READ_ALL_DATA: u32 = 6181;
pub const PG_WRITE_ALL_DATA: u32 = 6182;
pub const PG_MAINTAIN: u32 = 6337;

/// System catalogs: their owner may do anything, everyone may read them.
fn system_rel_acl(oid: u32) -> Acl {
    let mut acl = vec![AclItem {
        grantee: BOOTSTRAP_SUPERUSER_OID,
        grantor: BOOTSTRAP_SUPERUSER_OID,
        privs: ObjKind::Table.all(),
        goptions: 0,
    }];
    let name = super::system::SysTable::by_oid(oid).map(|t| t.name());
    // system_views.sql's REVOKE / GRANT on some views.
    match name {
        Some(
            "pg_shmem_allocations"
            | "pg_shmem_allocations_numa"
            | "pg_backend_memory_contexts"
            | "pg_aios",
        ) => {
            acl.push(AclItem {
                grantee: 3375, // pg_read_all_stats
                grantor: BOOTSTRAP_SUPERUSER_OID,
                privs: SELECT,
                goptions: 0,
            });
            return acl;
        }
        Some(
            "pg_shadow" | "pg_file_settings" | "pg_hba_file_rules" | "pg_ident_file_mappings"
            | "pg_config" | "pg_replication_origin_status",
        ) => return acl,
        Some("pg_settings") => {
            acl.push(AclItem {
                grantee: 0,
                grantor: BOOTSTRAP_SUPERUSER_OID,
                privs: SELECT | UPDATE,
                goptions: 0,
            });
            return acl;
        }
        _ => {}
    }
    // pg_authid, pg_largeobject, pg_statistic, pg_statistic_ext_data and pg_user_mapping
    // are readable by superusers only.
    if !matches!(oid, 1260 | 2613 | 2619 | 3429 | 1418) {
        acl.push(AclItem {
            grantee: 0,
            grantor: BOOTSTRAP_SUPERUSER_OID,
            privs: SELECT,
            goptions: 0,
        });
    }
    acl
}

impl Catalog {
    /// A relation's owner and ACL kind (tables, views, sequences, system catalogs).
    pub fn rel_owner(&self, oid: u32) -> Option<(u32, ObjKind)> {
        if let Some(t) = self.tables.get(&oid) {
            return Some((t.owner, ObjKind::Table));
        }
        if let Some(v) = self.views.get(&oid) {
            return Some((v.owner, ObjKind::Table));
        }
        if let Some(s) = self.sequences.get(&oid) {
            return Some((s.owner, ObjKind::Sequence));
        }
        if let Some(i) = self.indexes.get(&oid) {
            return self.index_table(i.table).map(|t| (t.owner, ObjKind::Table));
        }
        if super::system::SysTable::by_oid(oid).is_some() {
            return Some((BOOTSTRAP_SUPERUSER_OID, ObjKind::Table));
        }
        None
    }

    /// A relation's ACL, the default (`acldefault('r')`) when none is stored.
    pub fn rel_acl(&self, oid: u32, owner: u32, kind: ObjKind) -> Acl {
        match self.acl(kind, oid, 0) {
            Some(a) => a.clone(),
            None if oid < super::FIRST_USER_OID => system_rel_acl(oid),
            None => default_acl(kind, owner),
        }
    }

    /// pg_class_aclmask: (privileges, grant options) `role` holds on a relation.
    pub fn rel_mask(&self, oid: u32, role: u32, db_owner: u32) -> Option<(u32, u32)> {
        let (owner, kind) = self.rel_owner(oid)?;
        if self.is_superuser(role) {
            return Some((kind.all(), kind.all()));
        }
        let acl = self.rel_acl(oid, owner, kind);
        let (mut p, g) = self.aclmask(&acl, role, owner, db_owner);
        let roles = self.roles_of(role, true, db_owner);
        if roles.contains(&PG_READ_ALL_DATA) {
            p |= SELECT | if kind == ObjKind::Sequence { USAGE } else { 0 };
        }
        if roles.contains(&PG_WRITE_ALL_DATA) {
            p |= INSERT | UPDATE | DELETE;
        }
        if roles.contains(&PG_MAINTAIN) {
            p |= MAINTAIN;
        }
        Some((p & kind.all(), g & kind.all()))
    }

    /// Column privileges `role` holds through the column's own ACL (not the table's).
    pub fn col_mask(&self, rel: u32, attnum: i32, role: u32, db_owner: u32) -> (u32, u32) {
        let Some((owner, _)) = self.rel_owner(rel) else {
            return (0, 0);
        };
        if self.is_superuser(role) {
            return (COLUMN_PRIVS, COLUMN_PRIVS);
        }
        match self.acl(ObjKind::Table, rel, attnum) {
            Some(a) => {
                let (p, g) = self.aclmask(a, role, owner, db_owner);
                (p & COLUMN_PRIVS, g & COLUMN_PRIVS)
            }
            None => (0, 0),
        }
    }

    /// The owner of a non-relation object with an ACL (None if it doesn't exist).
    pub fn obj_owner(&self, kind: ObjKind, oid: u32, db_owner: u32) -> Option<u32> {
        match kind {
            ObjKind::Schema => self.schemas.get(&oid).map(|s| s.owner),
            ObjKind::Function => match self.functions.get(&oid) {
                Some(f) => Some(f.owner),
                None => Some(BOOTSTRAP_SUPERUSER_OID),
            },
            ObjKind::Type => self
                .enums
                .get(&oid)
                .map(|e| e.owner)
                .or_else(|| self.composites.get(&oid).map(|c| c.owner))
                .or_else(|| self.domains.get(&oid).map(|d| d.owner))
                .or_else(|| self.base_types.get(&oid).map(|b| b.owner))
                .or_else(|| {
                    self.ranges
                        .values()
                        .find(|r| r.oid == oid || r.multi_oid == oid)
                        .map(|r| r.owner)
                })
                .or(Some(BOOTSTRAP_SUPERUSER_OID)),
            ObjKind::Database => Some(db_owner),
            ObjKind::ForeignDataWrapper => {
                Some(self.fdws.get(&oid).map_or(BOOTSTRAP_SUPERUSER_OID, |f| f.owner))
            }
            ObjKind::ForeignServer => {
                Some(self.servers.get(&oid).map_or(BOOTSTRAP_SUPERUSER_OID, |f| f.owner))
            }
            ObjKind::Language => {
                Some(self.languages.get(&oid).map_or(BOOTSTRAP_SUPERUSER_OID, |l| l.owner))
            }
            ObjKind::Tablespace => Some(self.tablespace_owner(oid)),
            _ => Some(BOOTSTRAP_SUPERUSER_OID),
        }
    }

    /// (privileges, grant options) `role` holds on a schema, function, type, database, ….
    pub fn obj_mask(&self, kind: ObjKind, oid: u32, role: u32, db_owner: u32) -> (u32, u32) {
        let owner = self
            .obj_owner(kind, oid, db_owner)
            .unwrap_or(BOOTSTRAP_SUPERUSER_OID);
        if self.is_superuser(role) {
            return (kind.all(), kind.all());
        }
        let acl = match self.acl(kind, oid, 0) {
            Some(a) => a.clone(),
            // pg_catalog and information_schema: everyone may look things up.
            None if kind == ObjKind::Schema
                && (oid == super::PG_CATALOG_OID || oid == super::INFORMATION_SCHEMA_OID) =>
            {
                vec![
                    AclItem {
                        grantee: owner,
                        grantor: owner,
                        privs: USAGE | CREATE,
                        goptions: 0,
                    },
                    AclItem {
                        grantee: 0,
                        grantor: owner,
                        privs: USAGE,
                        goptions: 0,
                    },
                ]
            }
            None => initial_acl(kind, oid, owner),
        };
        let (mut p, g) = self.aclmask(&acl, role, owner, db_owner);
        if kind == ObjKind::Schema {
            let roles = self.roles_of(role, true, db_owner);
            if roles.contains(&PG_READ_ALL_DATA) || roles.contains(&PG_WRITE_ALL_DATA) {
                p |= USAGE;
            }
        }
        (p & kind.all(), g & kind.all())
    }
}
