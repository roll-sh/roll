//! `geo_shape` doc values in scripts (`doc['shape'].getCentroid()` & co.): the values
//! Elasticsearch 9.5.5 answered for the polygon `[[10,10],[20,10],[20,20],[10,20],[10,10]]`.

use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Settings};
use serde_json::{json, Value as Json};

struct Shape {
    empty: bool,
}

impl Host for Shape {
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        if field != "s" {
            return Ok(false);
        }
        out.clear(DocKind::GeoShape);
        if !self.empty {
            // the doc values' encoded centroid, bounding box and label position
            out.geo.extend_from_slice(&[
                [14.999999986030161, 14.999999944120646],
                [19.99999998137355, 9.999999990686774],
                [9.999999990686774, 19.99999998137355],
                [14.999999986030161, 14.999999944120646],
            ]);
            out.longs.push(2);
        }
        Ok(true)
    }
}

fn run(src: &str, empty: bool) -> Json {
    let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let script = engine.compile(src, Context::Field).unwrap();
    let mut inst = script.instance();
    match inst.execute(&mut Shape { empty }) {
        Ok(v) => search_painless::xcontent::to_xcontent(&v).map(|s| serde_json::from_str(&s).unwrap()).unwrap_or(Json::Null),
        Err(e) => e.to_response()["error"]["caused_by"]["type"].clone(),
    }
}

#[test]
fn geo_shape_doc_values() {
    assert_eq!(run("doc['s'].getCentroid()", false), json!({"lat": 14.999999986030161, "lon": 14.999999944120646}));
    assert_eq!(
        run("doc['s'].getBoundingBox()", false),
        json!({"top_left": {"lat": 19.99999998137355, "lon": 9.999999990686774}, "bottom_right": {"lat": 9.999999990686774, "lon": 19.99999998137355}})
    );
    assert_eq!(run("doc['s'].getDimensionalType()", false), json!(2));
    assert_eq!(run("doc['s'].getLabelPosition()", false), json!({"lat": 14.999999986030161, "lon": 14.999999944120646}));
    // spherical mercator with Lucene's sloppy cos and Elasticsearch's fast log: to the bit
    assert_eq!(run("doc['s'].getMercatorWidth()", false), json!(1113194.9067410342));
    assert_eq!(run("doc['s'].getMercatorHeight()", false), json!(1154140.9508152534));
    assert_eq!(run("doc['s'].size()", false), json!(1));
    assert_eq!(run("doc['s'].getValue() != null && doc['s'].get(0) != null", false), json!(true));
    // no shape
    assert_eq!(run("doc['s'].size()", true), json!(0));
    assert_eq!(run("doc['s'].getCentroid() == null", true), json!(true));
    assert_eq!(run("doc['s'].getDimensionalType()", true), json!(-1));
    assert_eq!(run("doc['s'].getMercatorWidth()", true), json!("null_pointer_exception"));
}
