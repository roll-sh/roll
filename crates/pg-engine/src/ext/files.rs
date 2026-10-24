//! The control files and scripts of the extensions the engine implements, as PostgreSQL
//! 18.6's contrib ships them (REL_18_6). Generated: `gen-ext-files.sh` copies them in.

pub(super) const FILES: &[(&str, &str)] = &[
    ("pgcrypto--1.0--1.1.sql", include_str!("files/pgcrypto--1.0--1.1.sql")),
    ("pgcrypto--1.1--1.2.sql", include_str!("files/pgcrypto--1.1--1.2.sql")),
    ("pgcrypto--1.2--1.3.sql", include_str!("files/pgcrypto--1.2--1.3.sql")),
    ("pgcrypto--1.3--1.4.sql", include_str!("files/pgcrypto--1.3--1.4.sql")),
    ("pgcrypto--1.3.sql", include_str!("files/pgcrypto--1.3.sql")),
    ("pgcrypto.control", include_str!("files/pgcrypto.control")),
    ("plpgsql--1.0.sql", include_str!("files/plpgsql--1.0.sql")),
    ("plpgsql.control", include_str!("files/plpgsql.control")),
    ("uuid-ossp--1.0--1.1.sql", include_str!("files/uuid-ossp--1.0--1.1.sql")),
    ("uuid-ossp--1.1.sql", include_str!("files/uuid-ossp--1.1.sql")),
    ("uuid-ossp.control", include_str!("files/uuid-ossp.control")),
];
