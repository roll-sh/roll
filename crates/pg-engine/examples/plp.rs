fn main() {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).unwrap();
    match pg_query::parse_plpgsql(&s) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => println!("ERR {e}"),
    }
}
