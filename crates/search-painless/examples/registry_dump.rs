//! Prints every allowlisted member (see `lookup::dump`); used to diff the registry against
//! Elasticsearch's allowlist files during development.
fn main() {
    for l in search_painless::registry_dump() {
        println!("{l}");
    }
}
