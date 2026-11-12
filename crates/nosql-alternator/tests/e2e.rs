//! End to end over HTTP with the AWS SDK for Rust: every operation the SDK can send to
//! Alternator, signed with SigV4, against the in-memory and the engine backend. (The Streams API is in
//! `tests/http.rs`: the DynamoDB Streams SDK isn't a dependency of the workspace.)

mod common;

use std::collections::HashMap;

use aws_sdk_dynamodb::error::ProvideErrorMetadata;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::*;
use common::{Kind, client, client_as, start_on};

type Item = HashMap<String, AttributeValue>;

fn s(v: &str) -> AttributeValue {
    AttributeValue::S(v.to_string())
}

fn n(v: &str) -> AttributeValue {
    AttributeValue::N(v.to_string())
}

fn item(pairs: &[(&str, AttributeValue)]) -> Item {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn def(name: &str, ty: ScalarAttributeType) -> AttributeDefinition {
    AttributeDefinition::builder()
        .attribute_name(name)
        .attribute_type(ty)
        .build()
        .unwrap()
}

fn key(name: &str, ty: KeyType) -> KeySchemaElement {
    KeySchemaElement::builder()
        .attribute_name(name)
        .key_type(ty)
        .build()
        .unwrap()
}

/// A table keyed (p S, c N), on demand.
async fn table(c: &aws_sdk_dynamodb::Client, name: &str) {
    c.create_table()
        .table_name(name)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("p", KeyType::Hash))
        .key_schema(key("c", KeyType::Range))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .attribute_definitions(def("c", ScalarAttributeType::N))
        .send()
        .await
        .unwrap();
}

async fn put(c: &aws_sdk_dynamodb::Client, t: &str, it: Item) {
    c.put_item()
        .table_name(t)
        .set_item(Some(it))
        .send()
        .await
        .unwrap();
}

async fn tables_lifecycle(kind: Kind) {
    let (addr, _) = start_on(kind, true).await;
    let c = client(addr);
    let out = c
        .create_table()
        .table_name("users")
        .key_schema(key("p", KeyType::Hash))
        .key_schema(key("c", KeyType::Range))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .attribute_definitions(def("c", ScalarAttributeType::N))
        .attribute_definitions(def("g", ScalarAttributeType::S))
        .attribute_definitions(def("l", ScalarAttributeType::B))
        .provisioned_throughput(
            ProvisionedThroughput::builder()
                .read_capacity_units(5)
                .write_capacity_units(7)
                .build()
                .unwrap(),
        )
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("by_g")
                .key_schema(key("g", KeyType::Hash))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .local_secondary_indexes(
            LocalSecondaryIndex::builder()
                .index_name("by_l")
                .key_schema(key("p", KeyType::Hash))
                .key_schema(key("l", KeyType::Range))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::KeysOnly)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .stream_specification(
            StreamSpecification::builder()
                .stream_enabled(true)
                .stream_view_type(StreamViewType::NewAndOldImages)
                .build()
                .unwrap(),
        )
        .tags(Tag::builder().key("team").value("db").build().unwrap())
        .send()
        .await
        .unwrap();
    let d = out.table_description().unwrap();
    assert_eq!(d.table_status(), Some(&TableStatus::Active));
    assert!(d.latest_stream_arn().is_some());

    let d = c.describe_table().table_name("users").send().await.unwrap();
    let t = d.table().unwrap();
    assert_eq!(t.table_name(), Some("users"));
    assert_eq!(t.key_schema().len(), 2);
    assert_eq!(t.attribute_definitions().len(), 4);
    assert_eq!(t.global_secondary_indexes()[0].index_name(), Some("by_g"));
    assert_eq!(
        t.global_secondary_indexes()[0].index_status(),
        Some(&IndexStatus::Active)
    );
    assert_eq!(t.local_secondary_indexes()[0].index_name(), Some("by_l"));
    assert_eq!(
        t.provisioned_throughput().unwrap().read_capacity_units(),
        Some(5)
    );
    assert_eq!(
        t.billing_mode_summary().unwrap().billing_mode(),
        Some(&BillingMode::Provisioned)
    );
    assert_eq!(
        t.table_arn(),
        Some("arn:scylla:alternator:alternator_users:scylla:table/users")
    );
    assert_eq!(
        t.stream_specification().unwrap().stream_view_type(),
        Some(&StreamViewType::NewAndOldImages)
    );

    // Already exists.
    let e = c
        .create_table()
        .table_name("users")
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("p", KeyType::Hash))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(e.is_resource_in_use_exception());
    assert_eq!(e.message(), Some("Table users already exists"));

    // Validation errors keep ScyllaDB's texts.
    let e = c
        .create_table()
        .table_name("ab")
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("p", KeyType::Hash))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(e.code(), Some("ValidationException"));
    assert_eq!(
        e.message(),
        Some("TableName must be at least 3 characters long and at most 192 characters long")
    );

    // UpdateTable: a new GSI (CREATING at first), stream off, then delete the GSI.
    c.update_table()
        .table_name("users")
        .attribute_definitions(def("h", ScalarAttributeType::N))
        .global_secondary_index_updates(
            GlobalSecondaryIndexUpdate::builder()
                .create(
                    CreateGlobalSecondaryIndexAction::builder()
                        .index_name("by_h")
                        .key_schema(key("h", KeyType::Hash))
                        .projection(
                            Projection::builder()
                                .projection_type(ProjectionType::All)
                                .build(),
                        )
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let t = c.describe_table().table_name("users").send().await.unwrap();
    let gsis = t.table().unwrap().global_secondary_indexes();
    let by_h = gsis
        .iter()
        .find(|g| g.index_name() == Some("by_h"))
        .unwrap();
    assert_eq!(by_h.index_status(), Some(&IndexStatus::Creating));
    // Added by UpdateTable, a hash-only GSI gets the table's hash key as its range key.
    assert_eq!(by_h.key_schema().len(), 2);
    c.update_table()
        .table_name("users")
        .stream_specification(
            StreamSpecification::builder()
                .stream_enabled(false)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    c.update_table()
        .table_name("users")
        .global_secondary_index_updates(
            GlobalSecondaryIndexUpdate::builder()
                .delete(
                    DeleteGlobalSecondaryIndexAction::builder()
                        .index_name("by_h")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let t = c.describe_table().table_name("users").send().await.unwrap();
    assert_eq!(t.table().unwrap().global_secondary_indexes().len(), 1);
    assert!(
        !t.table()
            .unwrap()
            .stream_specification()
            .unwrap()
            .stream_enabled()
    );

    // ListTables with pagination.
    for name in ["t01", "t02", "t03"] {
        table(&c, name).await;
    }
    let page = c.list_tables().limit(2).send().await.unwrap();
    assert_eq!(page.table_names().len(), 2);
    let next = page.last_evaluated_table_name().unwrap().to_string();
    let rest = c
        .list_tables()
        .exclusive_start_table_name(next)
        .send()
        .await
        .unwrap();
    assert_eq!(rest.table_names().len(), 2);
    assert!(rest.last_evaluated_table_name().is_none());
    let e = c
        .list_tables()
        .limit(101)
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("Limit must be greater than 0 and no greater than 100")
    );

    // DeleteTable, then the table is gone.
    let d = c.delete_table().table_name("users").send().await.unwrap();
    assert_eq!(
        d.table_description().unwrap().table_status(),
        Some(&TableStatus::Deleting)
    );
    let e = c
        .describe_table()
        .table_name("users")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(e.is_resource_not_found_exception());
    assert_eq!(
        e.message(),
        Some("Requested resource not found: Table: users not found")
    );
}

async fn items_put_get_delete(kind: Kind) {
    let (addr, _) = start_on(kind, true).await;
    let c = client(addr);
    table(&c, "items").await;
    let full = item(&[
        ("p", s("k")),
        ("c", n("1.50")),
        ("s", s("hello")),
        ("n", n("-12.3e5")),
        ("b", AttributeValue::B(Blob::new(vec![1, 2, 3]))),
        ("t", AttributeValue::Bool(true)),
        ("z", AttributeValue::Null(true)),
        (
            "m",
            AttributeValue::M(item(&[("x", n("1")), ("y", s("q"))])),
        ),
        ("l", AttributeValue::L(vec![n("1"), s("a")])),
        ("ss", AttributeValue::Ss(vec!["b".into(), "a".into()])),
        ("ns", AttributeValue::Ns(vec!["3".into(), "1.0".into()])),
        (
            "bs",
            AttributeValue::Bs(vec![Blob::new(vec![1]), Blob::new(vec![0])]),
        ),
        ("e", s("")),
    ]);
    let out = c
        .put_item()
        .table_name("items")
        .set_item(Some(full.clone()))
        .return_consumed_capacity(ReturnConsumedCapacity::Total)
        .send()
        .await
        .unwrap();
    assert_eq!(out.consumed_capacity().unwrap().capacity_units(), Some(1.0));
    let got = c
        .get_item()
        .table_name("items")
        .key("p", s("k"))
        .key("c", n("1.5"))
        .consistent_read(true)
        .return_consumed_capacity(ReturnConsumedCapacity::Total)
        .send()
        .await
        .unwrap();
    let it = got.item().unwrap();
    // Top-level numbers come back in ScyllaDB's canonical form; nested ones verbatim.
    assert_eq!(it["c"], n("1.5"));
    assert_eq!(it["n"], n("-1230000"));
    assert_eq!(it["ns"], AttributeValue::Ns(vec!["3".into(), "1.0".into()]));
    assert_eq!(it["b"], AttributeValue::B(Blob::new(vec![1, 2, 3])));
    assert_eq!(it["e"], s(""));
    assert_eq!(it.len(), full.len());
    assert_eq!(got.consumed_capacity().unwrap().capacity_units(), Some(1.0));

    // Projection with nested paths and names.
    let got = c
        .get_item()
        .table_name("items")
        .key("p", s("k"))
        .key("c", n("1.5"))
        .projection_expression("#m.x, l[1], s")
        .expression_attribute_names("#m", "m")
        .send()
        .await
        .unwrap();
    let it = got.item().unwrap();
    assert_eq!(it["m"], AttributeValue::M(item(&[("x", n("1"))])));
    assert_eq!(it["l"], AttributeValue::L(vec![s("a")]));
    assert_eq!(it.len(), 3);
    let got = c
        .get_item()
        .table_name("items")
        .key("p", s("k"))
        .key("c", n("1.5"))
        .attributes_to_get("s")
        .send()
        .await
        .unwrap();
    assert_eq!(got.item().unwrap().len(), 1);

    // Conditions: failing, with the old item returned; succeeding with ALL_OLD.
    let e = c
        .put_item()
        .table_name("items")
        .item("p", s("k"))
        .item("c", n("1.5"))
        .condition_expression("attribute_not_exists(p)")
        .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld)
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(e.is_conditional_check_failed_exception());
    assert_eq!(e.message(), Some("The conditional request failed"));
    let out = c
        .put_item()
        .table_name("items")
        .item("p", s("k"))
        .item("c", n("1.5"))
        .item("v", n("2"))
        .condition_expression(
            "s = :s AND size(l) = :two AND contains(ss, :a) AND begins_with(s, :he)",
        )
        .expression_attribute_values(":s", s("hello"))
        .expression_attribute_values(":two", n("2"))
        .expression_attribute_values(":a", s("a"))
        .expression_attribute_values(":he", s("he"))
        .return_values(ReturnValue::AllOld)
        .send()
        .await
        .unwrap();
    assert_eq!(out.attributes().unwrap()["s"], s("hello"));

    // Legacy Expected.
    let e = c
        .put_item()
        .table_name("items")
        .item("p", s("k"))
        .item("c", n("1.5"))
        .expected("v", ExpectedAttributeValue::builder().value(n("3")).build())
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(e.is_conditional_check_failed_exception());
    c.put_item()
        .table_name("items")
        .item("p", s("k"))
        .item("c", n("1.5"))
        .item("v", n("3"))
        .expected(
            "v",
            ExpectedAttributeValue::builder()
                .comparison_operator(ComparisonOperator::Between)
                .attribute_value_list(n("1"))
                .attribute_value_list(n("2"))
                .build(),
        )
        .expected(
            "nope",
            ExpectedAttributeValue::builder().exists(false).build(),
        )
        .conditional_operator(ConditionalOperator::And)
        .send()
        .await
        .unwrap();

    // DeleteItem with a condition and ALL_OLD.
    let e = c
        .delete_item()
        .table_name("items")
        .key("p", s("k"))
        .key("c", n("1.5"))
        .condition_expression("v > :v")
        .expression_attribute_values(":v", n("3"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(e.is_conditional_check_failed_exception());
    let out = c
        .delete_item()
        .table_name("items")
        .key("p", s("k"))
        .key("c", n("1.5"))
        .return_values(ReturnValue::AllOld)
        .send()
        .await
        .unwrap();
    assert_eq!(out.attributes().unwrap()["v"], n("3"));
    let got = c
        .get_item()
        .table_name("items")
        .key("p", s("k"))
        .key("c", n("1.5"))
        .send()
        .await
        .unwrap();
    assert!(got.item().is_none());

    // Errors.
    let e = c
        .get_item()
        .table_name("nope")
        .key("p", s("k"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(e.is_resource_not_found_exception());
    let e = c
        .get_item()
        .table_name("items")
        .key("p", n("1"))
        .key("c", n("1"))
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        e.into_service_error().message(),
        Some("Type mismatch: expected type S for key column p, got type \"N\"")
    );
    let e = c
        .put_item()
        .table_name("items")
        .item("p", s("k"))
        .item("c", n("1"))
        .item("x", n("1e126"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some(
            "Number overflow: 1e126. Attempting to store a number with magnitude larger than supported range."
        )
    );
    let e = c
        .put_item()
        .table_name("items")
        .item("p", s("k"))
        .item("c", n("1"))
        .condition_expression("a = = :v")
        .expression_attribute_values(":v", n("1"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("Failed parsing ConditionExpression 'a = = :v': syntax error at char 4")
    );
}

async fn update_item_expressions_and_legacy(kind: Kind) {
    let (addr, _) = start_on(kind, true).await;
    let c = client(addr);
    table(&c, "upd").await;
    put(
        &c,
        "upd",
        item(&[
            ("p", s("k")),
            ("c", n("1")),
            ("a", n("5")),
            ("l", AttributeValue::L(vec![n("0"), n("1"), n("2")])),
            ("m", AttributeValue::M(item(&[("x", s("y"))]))),
            ("ss", AttributeValue::Ss(vec!["a".into(), "b".into()])),
            ("ds", AttributeValue::Ss(vec!["a".into(), "b".into()])),
        ]),
    )
    .await;
    let out = c
        .update_item()
        .table_name("upd")
        .key("p", s("k"))
        .key("c", n("1"))
        .update_expression(
            "SET a = a + :one, l[5] = :z, m.#new = if_not_exists(m.q, :z), q = list_append(l, :more) \
             REMOVE l[0] ADD ss :ss, cnt :one DELETE ds :del",
        )
        .expression_attribute_names("#new", "w")
        .expression_attribute_values(":one", n("1"))
        .expression_attribute_values(":z", s("z"))
        .expression_attribute_values(":more", AttributeValue::L(vec![n("9")]))
        .expression_attribute_values(":ss", AttributeValue::Ss(vec!["c".into()]))
        .expression_attribute_values(":del", AttributeValue::Ss(vec!["a".into()]))
        .condition_expression("a = :five")
        .expression_attribute_values(":five", n("5"))
        .return_values(ReturnValue::AllNew)
        .send()
        .await
        .unwrap();
    let a = out.attributes().unwrap();
    assert_eq!(a["a"], n("6"));
    assert_eq!(a["l"], AttributeValue::L(vec![n("1"), n("2"), s("z")]));
    assert_eq!(
        a["m"],
        AttributeValue::M(item(&[("x", s("y")), ("w", s("z"))]))
    );
    assert_eq!(
        a["q"],
        AttributeValue::L(vec![n("0"), n("1"), n("2"), n("9")])
    );
    assert_eq!(
        a["ss"],
        AttributeValue::Ss(vec!["a".into(), "b".into(), "c".into()])
    );
    assert_eq!(a["ds"], AttributeValue::Ss(vec!["b".into()]));
    assert_eq!(a["cnt"], n("1"));

    for (rv, want) in [
        (ReturnValue::UpdatedOld, Some(n("6"))),
        (ReturnValue::UpdatedNew, Some(n("8"))),
        (ReturnValue::AllOld, Some(n("8"))),
        (ReturnValue::None, None),
    ] {
        let out = c
            .update_item()
            .table_name("upd")
            .key("p", s("k"))
            .key("c", n("1"))
            .update_expression("SET a = a + :one")
            .expression_attribute_values(":one", n("1"))
            .return_values(rv)
            .send()
            .await
            .unwrap();
        assert_eq!(out.attributes().and_then(|a| a.get("a")).cloned(), want);
    }

    // Upsert of a new item, and errors.
    c.update_item()
        .table_name("upd")
        .key("p", s("new"))
        .key("c", n("2"))
        .update_expression("SET x = :v")
        .expression_attribute_values(":v", s("v"))
        .send()
        .await
        .unwrap();
    let e = c
        .update_item()
        .table_name("upd")
        .key("p", s("k"))
        .key("c", n("1"))
        .update_expression("SET a = :v, a = :v")
        .expression_attribute_values(":v", n("1"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("Invalid UpdateExpression: two document paths overlap at a")
    );
    let e = c
        .update_item()
        .table_name("upd")
        .key("p", s("k"))
        .key("c", n("1"))
        .update_expression("SET p = :v")
        .expression_attribute_values(":v", s("x"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(e.message(), Some("UpdateItem cannot update key column p"));
    let e = c
        .update_item()
        .table_name("upd")
        .key("p", s("k"))
        .key("c", n("1"))
        .update_expression("SET a = :v")
        .expression_attribute_values(":v", n("1"))
        .expression_attribute_values(":unused", n("1"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("ExpressionAttributeValues has spurious ':unused', not used in UpdateItem")
    );

    // Legacy AttributeUpdates with Expected.
    let out = c
        .update_item()
        .table_name("upd")
        .key("p", s("k"))
        .key("c", n("1"))
        .attribute_updates(
            "a",
            AttributeValueUpdate::builder()
                .action(AttributeAction::Add)
                .value(n("10"))
                .build(),
        )
        .attribute_updates(
            "m",
            AttributeValueUpdate::builder()
                .action(AttributeAction::Delete)
                .build(),
        )
        .attribute_updates(
            "w",
            AttributeValueUpdate::builder()
                .action(AttributeAction::Put)
                .value(s("put"))
                .build(),
        )
        .expected(
            "a",
            ExpectedAttributeValue::builder().value(n("10")).build(),
        )
        .return_values(ReturnValue::UpdatedNew)
        .send()
        .await
        .unwrap();
    let a = out.attributes().unwrap();
    assert_eq!(a["a"], n("20"));
    assert_eq!(a["w"], s("put"));
    assert!(!a.contains_key("m"));
}

async fn query_and_scan(kind: Kind) {
    let (addr, _) = start_on(kind, true).await;
    let c = client(addr);
    c.create_table()
        .table_name("qst")
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("p", KeyType::Hash))
        .key_schema(key("c", KeyType::Range))
        .attribute_definitions(def("p", ScalarAttributeType::S))
        .attribute_definitions(def("c", ScalarAttributeType::N))
        .attribute_definitions(def("g", ScalarAttributeType::S))
        .attribute_definitions(def("l", ScalarAttributeType::N))
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("by_g")
                .key_schema(key("g", KeyType::Hash))
                .key_schema(key("c", KeyType::Range))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .local_secondary_indexes(
            LocalSecondaryIndex::builder()
                .index_name("by_l")
                .key_schema(key("p", KeyType::Hash))
                .key_schema(key("l", KeyType::Range))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    for p in ["a", "b", "c"] {
        for i in 0..10 {
            put(
                &c,
                "qst",
                item(&[
                    ("p", s(p)),
                    ("c", n(&i.to_string())),
                    ("g", s(if i % 2 == 0 { "even" } else { "odd" })),
                    ("l", n(&(100 - i).to_string())),
                    ("v", n(&(i * 10).to_string())),
                ]),
            )
            .await;
        }
    }
    let q = c
        .query()
        .table_name("qst")
        .key_condition_expression("p = :p AND c BETWEEN :lo AND :hi")
        .expression_attribute_values(":p", s("a"))
        .expression_attribute_values(":lo", n("2"))
        .expression_attribute_values(":hi", n("7"))
        .filter_expression("v > :v")
        .expression_attribute_values(":v", n("30"))
        .projection_expression("c, v")
        .scan_index_forward(false)
        .send()
        .await
        .unwrap();
    assert_eq!(q.count(), 4);
    assert_eq!(q.scanned_count(), 6);
    let cs: Vec<_> = q.items().iter().map(|i| i["c"].clone()).collect();
    assert_eq!(cs, vec![n("7"), n("6"), n("5"), n("4")]);

    // Pagination with Limit and ExclusiveStartKey.
    let mut start = None;
    let mut seen = 0;
    let mut pages = 0;
    loop {
        let r = c
            .query()
            .table_name("qst")
            .key_condition_expression("p = :p")
            .expression_attribute_values(":p", s("b"))
            .limit(4)
            .set_exclusive_start_key(start.clone())
            .send()
            .await
            .unwrap();
        seen += r.count();
        pages += 1;
        match r.last_evaluated_key() {
            Some(k) => start = Some(k.clone()),
            None => break,
        }
    }
    assert_eq!((seen, pages), (10, 3));

    // Select COUNT, begins_with on a numeric key is refused.
    let r = c
        .query()
        .table_name("qst")
        .key_condition_expression("p = :p AND c >= :c")
        .expression_attribute_values(":p", s("c"))
        .expression_attribute_values(":c", n("5"))
        .select(Select::Count)
        .send()
        .await
        .unwrap();
    assert_eq!(r.count(), 5);
    assert!(r.items().is_empty());
    let e = c
        .query()
        .table_name("qst")
        .key_condition_expression("p = :p AND begins_with(c, :c)")
        .expression_attribute_values(":p", s("c"))
        .expression_attribute_values(":c", n("5"))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("KeyConditionExpression begins_with() not supported on type N")
    );

    // Indexes.
    let r = c
        .query()
        .table_name("qst")
        .index_name("by_g")
        .key_condition_expression("g = :g AND c < :c")
        .expression_attribute_values(":g", s("even"))
        .expression_attribute_values(":c", n("4"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.count(), 6);
    let r = c
        .query()
        .table_name("qst")
        .index_name("by_l")
        .key_condition_expression("p = :p AND l > :l")
        .expression_attribute_values(":p", s("a"))
        .expression_attribute_values(":l", n("97"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.items().iter().map(|i| i["l"].clone()).collect::<Vec<_>>(),
        vec![n("98"), n("99"), n("100")]
    );
    let e = c
        .query()
        .table_name("qst")
        .index_name("by_g")
        .key_condition_expression("g = :g")
        .expression_attribute_values(":g", s("even"))
        .consistent_read(true)
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("Consistent reads are not allowed on global indexes (GSI)")
    );

    // Legacy KeyConditions and QueryFilter.
    let r = c
        .query()
        .table_name("qst")
        .key_conditions(
            "p",
            Condition::builder()
                .comparison_operator(ComparisonOperator::Eq)
                .attribute_value_list(s("a"))
                .build()
                .unwrap(),
        )
        .key_conditions(
            "c",
            Condition::builder()
                .comparison_operator(ComparisonOperator::Le)
                .attribute_value_list(n("3"))
                .build()
                .unwrap(),
        )
        .query_filter(
            "g",
            Condition::builder()
                .comparison_operator(ComparisonOperator::Eq)
                .attribute_value_list(s("odd"))
                .build()
                .unwrap(),
        )
        .attributes_to_get("c")
        .send()
        .await
        .unwrap();
    assert_eq!(r.count(), 2);

    // Scan: everything, a filter, segments, ScanFilter.
    let r = c.scan().table_name("qst").send().await.unwrap();
    assert_eq!(r.count(), 30);
    let r = c
        .scan()
        .table_name("qst")
        .filter_expression("attribute_exists(v) AND NOT (g IN (:o))")
        .expression_attribute_values(":o", s("odd"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.count(), 15);
    let mut total = 0;
    for seg in 0..4 {
        total += c
            .scan()
            .table_name("qst")
            .segment(seg)
            .total_segments(4)
            .send()
            .await
            .unwrap()
            .count();
    }
    assert_eq!(total, 30);
    let r = c
        .scan()
        .table_name("qst")
        .scan_filter(
            "v",
            Condition::builder()
                .comparison_operator(ComparisonOperator::Ge)
                .attribute_value_list(n("80"))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.count(), 6);
    let mut paginated = 0;
    let mut p = c.scan().table_name("qst").limit(7).into_paginator().send();
    while let Some(page) = p.next().await {
        paginated += page.unwrap().count();
    }
    assert_eq!(paginated, 30);
}

async fn batches(kind: Kind) {
    let (addr, _) = start_on(kind, true).await;
    let c = client(addr);
    table(&c, "bt1").await;
    table(&c, "bt2").await;
    let wr = |p: &str, i: i32| {
        WriteRequest::builder()
            .put_request(
                PutRequest::builder()
                    .set_item(Some(item(&[
                        ("p", s(p)),
                        ("c", n(&i.to_string())),
                        ("x", s("v")),
                    ])))
                    .build()
                    .unwrap(),
            )
            .build()
    };
    let out = c
        .batch_write_item()
        .request_items("bt1", vec![wr("a", 1), wr("a", 2), wr("b", 1)])
        .request_items("bt2", vec![wr("z", 9)])
        .return_consumed_capacity(ReturnConsumedCapacity::Total)
        .send()
        .await
        .unwrap();
    assert!(out.unprocessed_items().unwrap().is_empty());
    assert_eq!(out.consumed_capacity().len(), 2);
    // Delete one, and duplicates are refused.
    c.batch_write_item()
        .request_items(
            "bt1",
            vec![
                WriteRequest::builder()
                    .delete_request(
                        DeleteRequest::builder()
                            .set_key(Some(item(&[("p", s("a")), ("c", n("2"))])))
                            .build()
                            .unwrap(),
                    )
                    .build(),
            ],
        )
        .send()
        .await
        .unwrap();
    let e = c
        .batch_write_item()
        .request_items("bt1", vec![wr("a", 1), wr("a", 1)])
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("Provided list of item keys contains duplicates")
    );
    let too_many: Vec<_> = (0..101).map(|i| wr("m", i)).collect();
    let e = c
        .batch_write_item()
        .request_items("bt1", too_many)
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some(
            "Invalid length of BatchWriteItem command, got 101 items, maximum is 100 (from configuration variable alternator_max_items_in_batch_write)"
        )
    );

    let keys = KeysAndAttributes::builder()
        .keys(item(&[("p", s("a")), ("c", n("1"))]))
        .keys(item(&[("p", s("a")), ("c", n("2"))]))
        .keys(item(&[("p", s("b")), ("c", n("1"))]))
        .projection_expression("p, c")
        .consistent_read(true)
        .build()
        .unwrap();
    let out = c
        .batch_get_item()
        .request_items("bt1", keys)
        .request_items(
            "bt2",
            KeysAndAttributes::builder()
                .keys(item(&[("p", s("z")), ("c", n("9"))]))
                .build()
                .unwrap(),
        )
        .return_consumed_capacity(ReturnConsumedCapacity::Total)
        .send()
        .await
        .unwrap();
    let r = out.responses().unwrap();
    assert_eq!(r["bt1"].len(), 2);
    assert_eq!(r["bt2"][0]["x"], s("v"));
    assert!(out.unprocessed_keys().unwrap().is_empty());
}

async fn tags_ttl_and_misc(kind: Kind) {
    let (addr, _) = start_on(kind, true).await;
    let c = client(addr);
    table(&c, "tagged").await;
    let arn = "arn:scylla:alternator:alternator_tagged:scylla:table/tagged";
    c.tag_resource()
        .resource_arn(arn)
        .tags(Tag::builder().key("b").value("2").build().unwrap())
        .tags(Tag::builder().key("a").value("1").build().unwrap())
        .send()
        .await
        .unwrap();
    let tags = c
        .list_tags_of_resource()
        .resource_arn(arn)
        .send()
        .await
        .unwrap();
    let keys: Vec<_> = tags.tags().iter().map(|t| t.key().to_string()).collect();
    assert_eq!(keys, ["a", "b"]);
    c.untag_resource()
        .resource_arn(arn)
        .tag_keys("a")
        .send()
        .await
        .unwrap();
    assert_eq!(
        c.list_tags_of_resource()
            .resource_arn(arn)
            .send()
            .await
            .unwrap()
            .tags()
            .len(),
        1
    );
    let e = c
        .tag_resource()
        .resource_arn(arn)
        .tags(Tag::builder().key("system:x").value("1").build().unwrap())
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(
        e.message(),
        Some("Tag key 'system:x' is reserved for internal use")
    );
    let e = c
        .list_tags_of_resource()
        .resource_arn("nope")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(e.code(), Some("AccessDeniedException"));

    c.update_time_to_live()
        .table_name("tagged")
        .time_to_live_specification(
            TimeToLiveSpecification::builder()
                .attribute_name("exp")
                .enabled(true)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let ttl = c
        .describe_time_to_live()
        .table_name("tagged")
        .send()
        .await
        .unwrap();
    let d = ttl.time_to_live_description().unwrap();
    assert_eq!(d.time_to_live_status(), Some(&TimeToLiveStatus::Enabled));
    assert_eq!(d.attribute_name(), Some("exp"));
    let e = c
        .update_time_to_live()
        .table_name("tagged")
        .time_to_live_specification(
            TimeToLiveSpecification::builder()
                .attribute_name("exp")
                .enabled(true)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(e.message(), Some("TTL is already enabled"));

    let b = c
        .describe_continuous_backups()
        .table_name("tagged")
        .send()
        .await
        .unwrap();
    assert_eq!(
        b.continuous_backups_description()
            .unwrap()
            .continuous_backups_status(),
        &ContinuousBackupsStatus::Disabled
    );
    let ep = c.describe_endpoints().send().await.unwrap();
    assert_eq!(ep.endpoints()[0].address(), addr.to_string());
    assert_eq!(ep.endpoints()[0].cache_period_in_minutes(), 1440);

    // Operations ScyllaDB doesn't implement.
    let e = c.list_backups().send().await.unwrap_err();
    assert_eq!(e.code(), Some("UnknownOperationException"));
    let e = c.transact_get_items().send().await.unwrap_err();
    assert_eq!(e.code(), Some("UnknownOperationException"));
}

async fn signatures(kind: Kind) {
    let (addr, _) = start_on(kind, true).await;
    // Wrong secret, unknown role, role that can't log in.
    let e = client_as(addr, "alice", "wrong")
        .list_tables()
        .send()
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some("UnrecognizedClientException"));
    let e = client_as(addr, "mallory", "x")
        .list_tables()
        .send()
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some("UnrecognizedClientException"));
    assert_eq!(e.message(), Some("User not found: mallory"));
    let e = client_as(addr, "bob", "bobsecret")
        .list_tables()
        .send()
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some("UnrecognizedClientException"));
    assert!(e.message().unwrap().contains("login=false"));
    // The right key works.
    assert!(client(addr).list_tables().send().await.is_ok());
    // Without enforcement any signature (or none) is accepted.
    let (open, _) = start_on(kind, false).await;
    assert!(
        client_as(open, "anyone", "anything")
            .list_tables()
            .send()
            .await
            .is_ok()
    );
}

/// Each test on both backends.
macro_rules! both {
    ($($name:ident),* $(,)?) => {
        mod memory {
            $(#[tokio::test]
            async fn $name() {
                super::$name(super::Kind::Memory).await
            })*
        }
        mod engine {
            $(#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $name() {
                super::$name(super::Kind::Engine).await
            })*
        }
    };
}

both!(
    tables_lifecycle,
    items_put_get_delete,
    update_item_expressions_and_legacy,
    query_and_scan,
    batches,
    tags_ttl_and_misc,
    signatures
);
