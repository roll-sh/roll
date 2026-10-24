//! The control files and scripts of the extensions the engine implements, as PostgreSQL
//! 18.6's contrib ships them (REL_18_6). Generated: `gen-ext-files.sh` copies them in.

pub(super) const FILES: &[(&str, &str)] = &[
    ("citext--1.0--1.1.sql", include_str!("files/citext--1.0--1.1.sql")),
    ("citext--1.1--1.2.sql", include_str!("files/citext--1.1--1.2.sql")),
    ("citext--1.2--1.3.sql", include_str!("files/citext--1.2--1.3.sql")),
    ("citext--1.3--1.4.sql", include_str!("files/citext--1.3--1.4.sql")),
    ("citext--1.4--1.5.sql", include_str!("files/citext--1.4--1.5.sql")),
    ("citext--1.4.sql", include_str!("files/citext--1.4.sql")),
    ("citext--1.5--1.6.sql", include_str!("files/citext--1.5--1.6.sql")),
    ("citext--1.6--1.7.sql", include_str!("files/citext--1.6--1.7.sql")),
    ("citext--1.7--1.8.sql", include_str!("files/citext--1.7--1.8.sql")),
    ("citext.control", include_str!("files/citext.control")),
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
