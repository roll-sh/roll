//! A TPC-H-like data generator (deterministic, dbgen's schema, row counts and value
//! distributions closely enough for the 22 queries to select and join what they do at a scale
//! factor), and the 22 queries with the specification's validation parameters.

/// splitmix64: one deterministic stream per (table, row), so a table can be generated in
/// chunks and orders and lineitems agree.
pub struct Rng(u64);

impl Rng {
    pub fn new(table: u64, row: u64) -> Rng {
        let mut r = Rng(table.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ row.wrapping_mul(0xD1B5_4A32_D192_ED03));
        r.next();
        r
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [lo, hi].
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i64
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "furiously", "sly", "careful", "blithely", "quickly", "fluffily", "slyly", "ironic",
    "final", "regular", "express", "pending", "bold", "even", "special", "unusual", "silent",
    "requests", "deposits", "packages", "accounts", "theodolites", "pinto", "beans", "foxes",
    "ideas", "instructions", "dependencies", "excuses", "platelets", "asymptotes", "courts",
    "dolphins", "multipliers", "sauternes", "warthogs", "frets", "dinos", "attainments",
    "somas", "gifts", "sheaves", "hockey", "players", "nag", "haggle", "sleep", "wake", "are",
    "cajole", "boost", "detect", "use", "integrate", "among", "across", "above", "according",
    "to", "the", "against", "along", "carefully", "daringly", "thinly",
];
const COLORS: &[&str] = &[
    "almond", "antique", "aquamarine", "azure", "beige", "bisque", "black", "blanched", "blue",
    "blush", "brown", "burlywood", "burnished", "chartreuse", "chiffon", "chocolate", "coral",
    "cornflower", "cornsilk", "cream", "cyan", "dark", "deep", "dim", "dodger", "drab",
    "firebrick", "floral", "forest", "frosted", "gainsboro", "ghost", "goldenrod", "green",
    "grey", "honeydew", "hot", "indian", "ivory", "khaki", "lace", "lavender", "lawn", "lemon",
    "light", "lime", "linen", "magenta", "maroon", "medium", "metallic", "midnight", "mint",
    "misty", "moccasin", "navajo", "navy", "olive", "orange", "orchid", "pale", "papaya",
    "peach", "peru", "pink", "plum", "powder", "puff", "purple", "red", "rose", "rosy", "royal",
    "saddle", "salmon", "sandy", "seashell", "sienna", "sky", "slate", "smoke", "snow",
    "spring", "steel", "tan", "thistle", "tomato", "turquoise", "violet", "wheat", "white",
    "yellow",
];
const TYPE1: &[&str] = &["STANDARD", "SMALL", "MEDIUM", "LARGE", "ECONOMY", "PROMO"];
const TYPE2: &[&str] = &["ANODIZED", "BURNISHED", "PLATED", "POLISHED", "BRUSHED"];
const TYPE3: &[&str] = &["TIN", "NICKEL", "BRASS", "STEEL", "COPPER"];
const CONT1: &[&str] = &["SM", "LG", "MED", "JUMBO", "WRAP"];
const CONT2: &[&str] = &["CASE", "BOX", "BAG", "JAR", "PKG", "PACK", "CAN", "DRUM"];
const SEGMENTS: &[&str] = &["AUTOMOBILE", "BUILDING", "FURNITURE", "MACHINERY", "HOUSEHOLD"];
const PRIORITIES: &[&str] = &["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
const INSTRUCT: &[&str] = &["DELIVER IN PERSON", "COLLECT COD", "NONE", "TAKE BACK RETURN"];
const MODES: &[&str] = &["REG AIR", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"];
const REGIONS: &[&str] = &["AFRICA", "AMERICA", "ASIA", "EUROPE", "MIDDLE EAST"];
const NATIONS: &[(&str, i64)] = &[
    ("ALGERIA", 0), ("ARGENTINA", 1), ("BRAZIL", 1), ("CANADA", 1), ("EGYPT", 4),
    ("ETHIOPIA", 0), ("FRANCE", 3), ("GERMANY", 3), ("INDIA", 2), ("INDONESIA", 2),
    ("IRAN", 4), ("IRAQ", 4), ("JAPAN", 2), ("JORDAN", 4), ("KENYA", 0), ("MOROCCO", 0),
    ("MOZAMBIQUE", 0), ("PERU", 1), ("CHINA", 2), ("ROMANIA", 3), ("SAUDI ARABIA", 4),
    ("VIETNAM", 2), ("RUSSIA", 3), ("UNITED KINGDOM", 3), ("UNITED STATES", 1),
];

pub const SCHEMA: &str = "
CREATE TABLE region (r_regionkey int NOT NULL, r_name char(25) NOT NULL, r_comment varchar(152));
CREATE TABLE nation (n_nationkey int NOT NULL, n_name char(25) NOT NULL, n_regionkey int NOT NULL, n_comment varchar(152));
CREATE TABLE supplier (s_suppkey int NOT NULL, s_name char(25) NOT NULL, s_address varchar(40) NOT NULL, s_nationkey int NOT NULL, s_phone char(15) NOT NULL, s_acctbal numeric(15,2) NOT NULL, s_comment varchar(101) NOT NULL);
CREATE TABLE part (p_partkey int NOT NULL, p_name varchar(55) NOT NULL, p_mfgr char(25) NOT NULL, p_brand char(10) NOT NULL, p_type varchar(25) NOT NULL, p_size int NOT NULL, p_container char(10) NOT NULL, p_retailprice numeric(15,2) NOT NULL, p_comment varchar(23) NOT NULL);
CREATE TABLE partsupp (ps_partkey int NOT NULL, ps_suppkey int NOT NULL, ps_availqty int NOT NULL, ps_supplycost numeric(15,2) NOT NULL, ps_comment varchar(199) NOT NULL);
CREATE TABLE customer (c_custkey int NOT NULL, c_name varchar(25) NOT NULL, c_address varchar(40) NOT NULL, c_nationkey int NOT NULL, c_phone char(15) NOT NULL, c_acctbal numeric(15,2) NOT NULL, c_mktsegment char(10) NOT NULL, c_comment varchar(117) NOT NULL);
CREATE TABLE orders (o_orderkey int NOT NULL, o_custkey int NOT NULL, o_orderstatus char(1) NOT NULL, o_totalprice numeric(15,2) NOT NULL, o_orderdate date NOT NULL, o_orderpriority char(15) NOT NULL, o_clerk char(15) NOT NULL, o_shippriority int NOT NULL, o_comment varchar(79) NOT NULL);
CREATE TABLE lineitem (l_orderkey int NOT NULL, l_partkey int NOT NULL, l_suppkey int NOT NULL, l_linenumber int NOT NULL, l_quantity numeric(15,2) NOT NULL, l_extendedprice numeric(15,2) NOT NULL, l_discount numeric(15,2) NOT NULL, l_tax numeric(15,2) NOT NULL, l_returnflag char(1) NOT NULL, l_linestatus char(1) NOT NULL, l_shipdate date NOT NULL, l_commitdate date NOT NULL, l_receiptdate date NOT NULL, l_shipinstruct char(25) NOT NULL, l_shipmode char(10) NOT NULL, l_comment varchar(44) NOT NULL);
";

/// Keys and indexes, made after loading (as dbgen-based kits do).
pub const INDEXES: &[&str] = &[
    "ALTER TABLE region ADD PRIMARY KEY (r_regionkey)",
    "ALTER TABLE nation ADD PRIMARY KEY (n_nationkey)",
    "ALTER TABLE supplier ADD PRIMARY KEY (s_suppkey)",
    "ALTER TABLE part ADD PRIMARY KEY (p_partkey)",
    "ALTER TABLE partsupp ADD PRIMARY KEY (ps_partkey, ps_suppkey)",
    "ALTER TABLE customer ADD PRIMARY KEY (c_custkey)",
    "ALTER TABLE orders ADD PRIMARY KEY (o_orderkey)",
    "ALTER TABLE lineitem ADD PRIMARY KEY (l_orderkey, l_linenumber)",
    "CREATE INDEX ON lineitem (l_shipdate)",
    "CREATE INDEX ON lineitem (l_partkey, l_suppkey)",
    "CREATE INDEX ON orders (o_custkey)",
    "CREATE INDEX ON orders (o_orderdate)",
    "CREATE INDEX ON partsupp (ps_suppkey)",
    "CREATE INDEX ON customer (c_nationkey)",
    "CREATE INDEX ON supplier (s_nationkey)",
];

pub const TABLES: &[&str] = &[
    "region", "nation", "supplier", "part", "partsupp", "customer", "orders", "lineitem",
];

fn text(r: &mut Rng, min: usize, max: usize, out: &mut String) {
    let len = r.range(min as i64, max as i64) as usize;
    let start = out.len();
    while out.len() - start < len {
        if out.len() > start {
            out.push(' ');
        }
        out.push_str(r.pick(WORDS));
    }
    out.truncate(start + len);
    while out.ends_with(' ') {
        out.pop();
    }
}

fn money(cents: i64, out: &mut String) {
    use std::fmt::Write;
    let sign = if cents < 0 { "-" } else { "" };
    let c = cents.abs();
    let _ = write!(out, "{sign}{}.{:02}", c / 100, c % 100);
}

fn phone(r: &mut Rng, nation: i64, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "{}-{}-{}-{}",
        nation + 10,
        r.range(100, 999),
        r.range(100, 999),
        r.range(1000, 9999)
    );
}

/// Days since 1992-01-01 as a date.
fn date(days: i64, out: &mut String) {
    let d = chrono::NaiveDate::from_ymd_opt(1992, 1, 1).unwrap() + chrono::Duration::days(days);
    use std::fmt::Write;
    let _ = write!(out, "{}", d.format("%Y-%m-%d"));
}

pub fn retail_cents(p: i64) -> i64 {
    90000 + ((p / 10) % 20001) + 100 * (p % 1000)
}

pub struct Scale {
    pub sf: f64,
}

impl Scale {
    pub fn rows(&self, table: &str) -> i64 {
        let sf = self.sf;
        let n = |base: f64| ((base * sf) as i64).max(1);
        match table {
            "region" => 5,
            "nation" => 25,
            "supplier" => n(10_000.0),
            "part" => n(200_000.0),
            "partsupp" => n(200_000.0) * 4,
            "customer" => n(150_000.0),
            // (Orders; lineitem follows its orders.)
            "orders" | "lineitem" => n(1_500_000.0),
            _ => 0,
        }
    }

    fn suppliers(&self) -> i64 {
        self.rows("supplier")
    }

    /// The i-th supplier of part p (dbgen's formula).
    fn supp_of(&self, p: i64, i: i64) -> i64 {
        let s = self.suppliers();
        (p + i * (s / 4 + (p - 1) / s)) % s + 1
    }

    fn order_key(i: i64) -> i64 {
        (i / 8) * 32 + i % 8 + 1
    }

    /// COPY text rows [from, to) of `table` (by its row number; lineitem by its orders').
    pub fn rows_text(&self, table: &str, from: i64, to: i64, out: &mut String) {
        use std::fmt::Write;
        for i in from..to {
            match table {
                "region" => {
                    let mut r = Rng::new(1, i as u64);
                    let _ = write!(out, "{i}\t{}\t", REGIONS[i as usize]);
                    text(&mut r, 31, 115, out);
                    out.push('\n');
                }
                "nation" => {
                    let mut r = Rng::new(2, i as u64);
                    let (name, region) = NATIONS[i as usize];
                    let _ = write!(out, "{i}\t{name}\t{region}\t");
                    text(&mut r, 31, 114, out);
                    out.push('\n');
                }
                "supplier" => {
                    let k = i + 1;
                    let mut r = Rng::new(3, k as u64);
                    let nation = r.range(0, 24);
                    let _ = write!(out, "{k}\tSupplier#{k:09}\t");
                    text(&mut r, 10, 40, out);
                    let _ = write!(out, "\t{nation}\t");
                    phone(&mut r, nation, out);
                    out.push('\t');
                    money(r.range(-99999, 999999), out);
                    out.push('\t');
                    if k % 1000 == 7 {
                        out.push_str("slyly Customer final Complaints");
                    } else {
                        text(&mut r, 25, 100, out);
                    }
                    out.push('\n');
                }
                "part" => {
                    let k = i + 1;
                    let mut r = Rng::new(4, k as u64);
                    let _ = write!(out, "{k}\t");
                    for w in 0..5 {
                        if w > 0 {
                            out.push(' ');
                        }
                        out.push_str(r.pick(COLORS));
                    }
                    let m = r.range(1, 5);
                    let n = r.range(1, 5);
                    let _ = write!(
                        out,
                        "\tManufacturer#{m}\tBrand#{m}{n}\t{} {} {}\t{}\t{} {}\t",
                        r.pick(TYPE1),
                        r.pick(TYPE2),
                        r.pick(TYPE3),
                        r.range(1, 50),
                        r.pick(CONT1),
                        r.pick(CONT2)
                    );
                    money(retail_cents(k), out);
                    out.push('\t');
                    text(&mut r, 5, 22, out);
                    out.push('\n');
                }
                "partsupp" => {
                    let p = i / 4 + 1;
                    let j = i % 4;
                    let mut r = Rng::new(5, i as u64);
                    let _ = write!(
                        out,
                        "{p}\t{}\t{}\t",
                        self.supp_of(p, j),
                        r.range(1, 9999)
                    );
                    money(r.range(100, 100000), out);
                    out.push('\t');
                    text(&mut r, 49, 198, out);
                    out.push('\n');
                }
                "customer" => {
                    let k = i + 1;
                    let mut r = Rng::new(6, k as u64);
                    let nation = r.range(0, 24);
                    let _ = write!(out, "{k}\tCustomer#{k:09}\t");
                    text(&mut r, 10, 40, out);
                    let _ = write!(out, "\t{nation}\t");
                    phone(&mut r, nation, out);
                    out.push('\t');
                    money(r.range(-99999, 999999), out);
                    let _ = write!(out, "\t{}\t", r.pick(SEGMENTS));
                    text(&mut r, 29, 116, out);
                    out.push('\n');
                }
                "orders" | "lineitem" => self.order(i, table == "orders", out),
                _ => unreachable!("{table}"),
            }
        }
    }

    fn order(&self, i: i64, orders: bool, out: &mut String) {
        use std::fmt::Write;
        let key = Self::order_key(i);
        let mut r = Rng::new(7, key as u64);
        let customers = self.rows("customer");
        let mut cust = r.range(1, customers);
        // dbgen: a third of the customers have no orders.
        if cust % 3 == 0 {
            cust = if cust > 1 { cust - 1 } else { 1 };
        }
        // 1992-01-01 .. 1998-08-02 minus 151 days.
        let odate = r.range(0, 2405 - 151);
        let prio = r.pick(PRIORITIES);
        let clerk = r.range(1, ((1000.0 * self.sf) as i64).max(1));
        let lines = r.range(1, 7);
        let parts = self.rows("part");
        let current = 1263; // 1995-06-17
        let mut total = 0i64;
        let mut n_f = 0;
        let mut li = String::new();
        for ln in 1..=lines {
            let p = r.range(1, parts);
            let s = self.supp_of(p, r.range(0, 3));
            let qty = r.range(1, 50);
            let ext = qty * retail_cents(p);
            let disc = r.range(0, 10);
            let tax = r.range(0, 8);
            let ship = odate + r.range(1, 121);
            let commit = odate + r.range(30, 90);
            let receipt = ship + r.range(1, 30);
            let rflag = if receipt <= current {
                if r.next().is_multiple_of(2) { "R" } else { "A" }
            } else {
                "N"
            };
            let lstatus = if ship > current { "O" } else { "F" };
            if lstatus == "F" {
                n_f += 1;
            }
            total += ext * (100 + tax) / 100 * (100 - disc) / 100;
            let instruct = r.pick(INSTRUCT);
            let mode = r.pick(MODES);
            if !orders {
                let _ = write!(li, "{key}\t{p}\t{s}\t{ln}\t{qty}.00\t");
                money(ext, &mut li);
                let _ = write!(li, "\t0.{disc:02}\t0.{tax:02}\t{rflag}\t{lstatus}\t");
                date(ship, &mut li);
                li.push('\t');
                date(commit, &mut li);
                li.push('\t');
                date(receipt, &mut li);
                let _ = write!(li, "\t{instruct}\t{mode}\t");
                text(&mut r, 10, 43, &mut li);
                li.push('\n');
            } else {
                // (Keep the stream in step with the lineitem pass.)
                let mut sink = String::new();
                text(&mut r, 10, 43, &mut sink);
            }
        }
        if orders {
            let status = if n_f == lines {
                "F"
            } else if n_f == 0 {
                "O"
            } else {
                "P"
            };
            let _ = write!(out, "{key}\t{cust}\t{status}\t");
            money(total, out);
            out.push('\t');
            date(odate, out);
            let _ = write!(out, "\t{prio}\tClerk#{clerk:09}\t0\t");
            if key % 97 == 3 {
                out.push_str("blithely special packages wake requests");
            } else {
                text(&mut r, 19, 78, out);
            }
            out.push('\n');
        } else {
            out.push_str(&li);
        }
    }
}

/// The 22 queries with the specification's validation parameters (Q15's view as a CTE).
pub const QUERIES: &[(&str, &str)] = &[
    ("q01", "select l_returnflag, l_linestatus, sum(l_quantity) as sum_qty, sum(l_extendedprice) as sum_base_price, sum(l_extendedprice * (1 - l_discount)) as sum_disc_price, sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) as sum_charge, avg(l_quantity) as avg_qty, avg(l_extendedprice) as avg_price, avg(l_discount) as avg_disc, count(*) as count_order from lineitem where l_shipdate <= date '1998-12-01' - interval '90' day group by l_returnflag, l_linestatus order by l_returnflag, l_linestatus"),
    ("q02", "select s_acctbal, s_name, n_name, p_partkey, p_mfgr, s_address, s_phone, s_comment from part, supplier, partsupp, nation, region where p_partkey = ps_partkey and s_suppkey = ps_suppkey and p_size = 15 and p_type like '%BRASS' and s_nationkey = n_nationkey and n_regionkey = r_regionkey and r_name = 'EUROPE' and ps_supplycost = (select min(ps_supplycost) from partsupp, supplier, nation, region where p_partkey = ps_partkey and s_suppkey = ps_suppkey and s_nationkey = n_nationkey and n_regionkey = r_regionkey and r_name = 'EUROPE') order by s_acctbal desc, n_name, s_name, p_partkey limit 100"),
    ("q03", "select l_orderkey, sum(l_extendedprice * (1 - l_discount)) as revenue, o_orderdate, o_shippriority from customer, orders, lineitem where c_mktsegment = 'BUILDING' and c_custkey = o_custkey and l_orderkey = o_orderkey and o_orderdate < date '1995-03-15' and l_shipdate > date '1995-03-15' group by l_orderkey, o_orderdate, o_shippriority order by revenue desc, o_orderdate limit 10"),
    ("q04", "select o_orderpriority, count(*) as order_count from orders where o_orderdate >= date '1993-07-01' and o_orderdate < date '1993-07-01' + interval '3' month and exists (select * from lineitem where l_orderkey = o_orderkey and l_commitdate < l_receiptdate) group by o_orderpriority order by o_orderpriority"),
    ("q05", "select n_name, sum(l_extendedprice * (1 - l_discount)) as revenue from customer, orders, lineitem, supplier, nation, region where c_custkey = o_custkey and l_orderkey = o_orderkey and l_suppkey = s_suppkey and c_nationkey = s_nationkey and s_nationkey = n_nationkey and n_regionkey = r_regionkey and r_name = 'ASIA' and o_orderdate >= date '1994-01-01' and o_orderdate < date '1994-01-01' + interval '1' year group by n_name order by revenue desc"),
    ("q06", "select sum(l_extendedprice * l_discount) as revenue from lineitem where l_shipdate >= date '1994-01-01' and l_shipdate < date '1994-01-01' + interval '1' year and l_discount between 0.06 - 0.01 and 0.06 + 0.01 and l_quantity < 24"),
    ("q07", "select supp_nation, cust_nation, l_year, sum(volume) as revenue from (select n1.n_name as supp_nation, n2.n_name as cust_nation, extract(year from l_shipdate) as l_year, l_extendedprice * (1 - l_discount) as volume from supplier, lineitem, orders, customer, nation n1, nation n2 where s_suppkey = l_suppkey and o_orderkey = l_orderkey and c_custkey = o_custkey and s_nationkey = n1.n_nationkey and c_nationkey = n2.n_nationkey and ((n1.n_name = 'FRANCE' and n2.n_name = 'GERMANY') or (n1.n_name = 'GERMANY' and n2.n_name = 'FRANCE')) and l_shipdate between date '1995-01-01' and date '1996-12-31') as shipping group by supp_nation, cust_nation, l_year order by supp_nation, cust_nation, l_year"),
    ("q08", "select o_year, sum(case when nation = 'BRAZIL' then volume else 0 end) / sum(volume) as mkt_share from (select extract(year from o_orderdate) as o_year, l_extendedprice * (1 - l_discount) as volume, n2.n_name as nation from part, supplier, lineitem, orders, customer, nation n1, nation n2, region where p_partkey = l_partkey and s_suppkey = l_suppkey and l_orderkey = o_orderkey and o_custkey = c_custkey and c_nationkey = n1.n_nationkey and n1.n_regionkey = r_regionkey and r_name = 'AMERICA' and s_nationkey = n2.n_nationkey and o_orderdate between date '1995-01-01' and date '1996-12-31' and p_type = 'ECONOMY ANODIZED STEEL') as all_nations group by o_year order by o_year"),
    ("q09", "select nation, o_year, sum(amount) as sum_profit from (select n_name as nation, extract(year from o_orderdate) as o_year, l_extendedprice * (1 - l_discount) - ps_supplycost * l_quantity as amount from part, supplier, lineitem, partsupp, orders, nation where s_suppkey = l_suppkey and ps_suppkey = l_suppkey and ps_partkey = l_partkey and p_partkey = l_partkey and o_orderkey = l_orderkey and s_nationkey = n_nationkey and p_name like '%green%') as profit group by nation, o_year order by nation, o_year desc"),
    ("q10", "select c_custkey, c_name, sum(l_extendedprice * (1 - l_discount)) as revenue, c_acctbal, n_name, c_address, c_phone, c_comment from customer, orders, lineitem, nation where c_custkey = o_custkey and l_orderkey = o_orderkey and o_orderdate >= date '1993-10-01' and o_orderdate < date '1993-10-01' + interval '3' month and l_returnflag = 'R' and c_nationkey = n_nationkey group by c_custkey, c_name, c_acctbal, c_phone, n_name, c_address, c_comment order by revenue desc limit 20"),
    ("q11", "select ps_partkey, sum(ps_supplycost * ps_availqty) as value from partsupp, supplier, nation where ps_suppkey = s_suppkey and s_nationkey = n_nationkey and n_name = 'GERMANY' group by ps_partkey having sum(ps_supplycost * ps_availqty) > (select sum(ps_supplycost * ps_availqty) * 0.0001 from partsupp, supplier, nation where ps_suppkey = s_suppkey and s_nationkey = n_nationkey and n_name = 'GERMANY') order by value desc"),
    ("q12", "select l_shipmode, sum(case when o_orderpriority = '1-URGENT' or o_orderpriority = '2-HIGH' then 1 else 0 end) as high_line_count, sum(case when o_orderpriority <> '1-URGENT' and o_orderpriority <> '2-HIGH' then 1 else 0 end) as low_line_count from orders, lineitem where o_orderkey = l_orderkey and l_shipmode in ('MAIL', 'SHIP') and l_commitdate < l_receiptdate and l_shipdate < l_commitdate and l_receiptdate >= date '1994-01-01' and l_receiptdate < date '1994-01-01' + interval '1' year group by l_shipmode order by l_shipmode"),
    ("q13", "select c_count, count(*) as custdist from (select c_custkey, count(o_orderkey) from customer left outer join orders on c_custkey = o_custkey and o_comment not like '%special%requests%' group by c_custkey) as c_orders (c_custkey, c_count) group by c_count order by custdist desc, c_count desc"),
    ("q14", "select 100.00 * sum(case when p_type like 'PROMO%' then l_extendedprice * (1 - l_discount) else 0 end) / sum(l_extendedprice * (1 - l_discount)) as promo_revenue from lineitem, part where l_partkey = p_partkey and l_shipdate >= date '1995-09-01' and l_shipdate < date '1995-09-01' + interval '1' month"),
    ("q15", "with revenue0 (supplier_no, total_revenue) as (select l_suppkey, sum(l_extendedprice * (1 - l_discount)) from lineitem where l_shipdate >= date '1996-01-01' and l_shipdate < date '1996-01-01' + interval '3' month group by l_suppkey) select s_suppkey, s_name, s_address, s_phone, total_revenue from supplier, revenue0 where s_suppkey = supplier_no and total_revenue = (select max(total_revenue) from revenue0) order by s_suppkey"),
    ("q16", "select p_brand, p_type, p_size, count(distinct ps_suppkey) as supplier_cnt from partsupp, part where p_partkey = ps_partkey and p_brand <> 'Brand#45' and p_type not like 'MEDIUM POLISHED%' and p_size in (49, 14, 23, 45, 19, 3, 36, 9) and ps_suppkey not in (select s_suppkey from supplier where s_comment like '%Customer%Complaints%') group by p_brand, p_type, p_size order by supplier_cnt desc, p_brand, p_type, p_size"),
    ("q17", "select sum(l_extendedprice) / 7.0 as avg_yearly from lineitem, part where p_partkey = l_partkey and p_brand = 'Brand#23' and p_container = 'MED BOX' and l_quantity < (select 0.2 * avg(l_quantity) from lineitem where l_partkey = p_partkey)"),
    ("q18", "select c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, sum(l_quantity) from customer, orders, lineitem where o_orderkey in (select l_orderkey from lineitem group by l_orderkey having sum(l_quantity) > 300) and c_custkey = o_custkey and o_orderkey = l_orderkey group by c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice order by o_totalprice desc, o_orderdate limit 100"),
    ("q19", "select sum(l_extendedprice* (1 - l_discount)) as revenue from lineitem, part where (p_partkey = l_partkey and p_brand = 'Brand#12' and p_container in ('SM CASE', 'SM BOX', 'SM PACK', 'SM PKG') and l_quantity >= 1 and l_quantity <= 1 + 10 and p_size between 1 and 5 and l_shipmode in ('AIR', 'REG AIR') and l_shipinstruct = 'DELIVER IN PERSON') or (p_partkey = l_partkey and p_brand = 'Brand#23' and p_container in ('MED BAG', 'MED BOX', 'MED PKG', 'MED PACK') and l_quantity >= 10 and l_quantity <= 10 + 10 and p_size between 1 and 10 and l_shipmode in ('AIR', 'REG AIR') and l_shipinstruct = 'DELIVER IN PERSON') or (p_partkey = l_partkey and p_brand = 'Brand#34' and p_container in ('LG CASE', 'LG BOX', 'LG PACK', 'LG PKG') and l_quantity >= 20 and l_quantity <= 20 + 10 and p_size between 1 and 15 and l_shipmode in ('AIR', 'REG AIR') and l_shipinstruct = 'DELIVER IN PERSON')"),
    ("q20", "select s_name, s_address from supplier, nation where s_suppkey in (select ps_suppkey from partsupp where ps_partkey in (select p_partkey from part where p_name like 'forest%') and ps_availqty > (select 0.5 * sum(l_quantity) from lineitem where l_partkey = ps_partkey and l_suppkey = ps_suppkey and l_shipdate >= date '1994-01-01' and l_shipdate < date '1994-01-01' + interval '1' year)) and s_nationkey = n_nationkey and n_name = 'CANADA' order by s_name"),
    ("q21", "select s_name, count(*) as numwait from supplier, lineitem l1, orders, nation where s_suppkey = l1.l_suppkey and o_orderkey = l1.l_orderkey and o_orderstatus = 'F' and l1.l_receiptdate > l1.l_commitdate and exists (select * from lineitem l2 where l2.l_orderkey = l1.l_orderkey and l2.l_suppkey <> l1.l_suppkey) and not exists (select * from lineitem l3 where l3.l_orderkey = l1.l_orderkey and l3.l_suppkey <> l1.l_suppkey and l3.l_receiptdate > l3.l_commitdate) and s_nationkey = n_nationkey and n_name = 'SAUDI ARABIA' group by s_name order by numwait desc, s_name limit 100"),
    ("q22", "select cntrycode, count(*) as numcust, sum(c_acctbal) as totacctbal from (select substring(c_phone from 1 for 2) as cntrycode, c_acctbal from customer where substring(c_phone from 1 for 2) in ('13', '31', '23', '29', '30', '18', '17') and c_acctbal > (select avg(c_acctbal) from customer where c_acctbal > 0.00 and substring(c_phone from 1 for 2) in ('13', '31', '23', '29', '30', '18', '17')) and not exists (select * from orders where o_custkey = c_custkey)) as custsale group by cntrycode order by cntrycode"),
];
