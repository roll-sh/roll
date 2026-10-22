#!/usr/bin/env python3
"""Captures aggregation fixtures from a real Elasticsearch 9.5.5.

Usage: python3 -I gen_fixtures.py [suite ...]   (ES_URL defaults to http://127.0.0.1:19200)

For every suite it creates the index `aggs-<suite>` with the suite's mapping and shard count,
indexes the suite's generated documents in one bulk (one segment per shard after a force
merge), records which shard each document landed on (in doc order), runs every case with
`size=0`, writes `aggs-<suite>.json` next to this script and deletes the index. The Rust tests
(`tests/es_fixtures.rs`) rebuild the same shards with search-index and compare responses.
"""

import json
import os
import random
import sys
import urllib.error
import urllib.request

ES = os.environ.get("ES_URL", "http://127.0.0.1:19200")
HERE = os.path.dirname(os.path.abspath(__file__))


def call(method, path, body=None, ndjson=False):
    data = None
    headers = {}
    if body is not None:
        if ndjson:
            data = body.encode()
            headers["content-type"] = "application/x-ndjson"
        else:
            data = (body if isinstance(body, str) else json.dumps(body)).encode()
            headers["content-type"] = "application/json"
    req = urllib.request.Request(ES + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        return json.loads(e.read().decode())


KEYWORDS = ["k%02d" % i for i in range(15)]
IPS = ["10.0.0.%d" % i for i in range(1, 30)] + ["192.168.1.%d" % i for i in range(1, 10)] + ["2001:db8::%x" % i for i in range(1, 8)]


def main_docs(n, seed):
    rnd = random.Random(seed)
    docs = []
    start = 1577232000  # 2019-12-25T00:00:00Z
    span = 1617580800 - start  # .. 2021-04-05
    for i in range(n):
        d = {}
        r = rnd.random()
        if r < 0.6:
            d["k"] = KEYWORDS[min(int(rnd.expovariate(0.35)), 14)]
        elif r < 0.8:
            d["k"] = [KEYWORDS[rnd.randrange(15)], KEYWORDS[rnd.randrange(15)]]
        d["tag"] = "t%03d" % rnd.randrange(300)
        r = rnd.random()
        if r < 0.7:
            d["n"] = rnd.randrange(0, 100)
        elif r < 0.85:
            d["n"] = [rnd.randrange(0, 100), rnd.randrange(0, 100)]
        d["i"] = rnd.randrange(-50, 51)
        if rnd.random() < 0.9:
            d["d"] = round(rnd.gauss(50, 20), 3)
        d["f"] = round(rnd.uniform(-10, 10), 2)
        if rnd.random() < 0.95:
            d["t"] = start + rnd.randrange(span)
        d["ip"] = IPS[rnd.randrange(len(IPS))]
        if rnd.random() < 0.9:
            d["g"] = {"lat": round(rnd.uniform(-60, 60), 5), "lon": round(rnd.uniform(-170, 170), 5)}
        d["b"] = rnd.random() < 0.4
        nest = []
        for _ in range(rnd.randrange(4)):
            nest.append({"x": rnd.randrange(10), "y": KEYWORDS[rnd.randrange(5)]})
        if nest:
            d["nest"] = nest
        if "t" in d:
            d["t"] = d["t"] * 1000 + rnd.randrange(1000)
        docs.append(d)
    # Text, from its own generator so the other fields keep their values.
    trnd = random.Random(seed + 1)
    words = ["alpha", "beta", "gamma", "delta", "omega", "river", "stone", "cloud", "fire", "ocean", "quiet", "storm"]
    boiler = "the quick brown fox jumps over the lazy dog"
    for d in docs:
        if trnd.random() < 0.85:
            text = " ".join(trnd.choice(words) for _ in range(trnd.randrange(3, 9)))
            if trnd.random() < 0.3:
                text = boiler + " " + text
            if d.get("b") and trnd.random() < 0.5:
                text += " storm warning"
            d["txt"] = text
    return docs


MAIN_MAPPING = {
    "properties": {
        "k": {"type": "keyword"},
        "tag": {"type": "keyword"},
        "n": {"type": "long"},
        "i": {"type": "integer"},
        "d": {"type": "double"},
        "f": {"type": "float"},
        "t": {"type": "date"},
        "ip": {"type": "ip"},
        "g": {"type": "geo_point"},
        "b": {"type": "boolean"},
        "nest": {"type": "nested", "properties": {"x": {"type": "long"}, "y": {"type": "keyword"}}},
        "txt": {"type": "text"},
    }
}


def main_cases():
    c = []

    def add(name, aggs, query=None, typed_keys=False):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": typed_keys})

    # Metrics.
    add("metrics_basic", {
        "avg_n": {"avg": {"field": "n"}}, "sum_d": {"sum": {"field": "d"}}, "min_i": {"min": {"field": "i"}},
        "max_f": {"max": {"field": "f"}}, "vc_k": {"value_count": {"field": "k"}}, "vc_ip": {"value_count": {"field": "ip"}},
        "vc_g": {"value_count": {"field": "g"}}, "stats_d": {"stats": {"field": "d"}},
        "es_n": {"extended_stats": {"field": "n", "sigma": 1.5}}, "avg_t": {"avg": {"field": "t"}},
        "min_t": {"min": {"field": "t"}}, "max_b": {"max": {"field": "b"}},
        "card_k": {"cardinality": {"field": "k"}}, "card_tag": {"cardinality": {"field": "tag"}},
        "card_n": {"cardinality": {"field": "n"}}, "card_d": {"cardinality": {"field": "d"}},
        "card_ip": {"cardinality": {"field": "ip", "precision_threshold": 10}},
        "card_tag_low": {"cardinality": {"field": "tag", "precision_threshold": 20}},
        "wavg": {"weighted_avg": {"value": {"field": "d"}, "weight": {"field": "i", "missing": 1}}},
    }, typed_keys=True)
    add("metrics_missing_and_unmapped", {
        "avg_missing": {"avg": {"field": "d", "missing": 1000}}, "sum_unmapped": {"sum": {"field": "nope"}},
        "min_unmapped_missing": {"min": {"field": "nope", "missing": 5}}, "stats_empty": {"stats": {"field": "nope"}},
        "es_empty": {"extended_stats": {"field": "nope"}}, "card_unmapped": {"cardinality": {"field": "nope"}},
        "avg_fmt": {"avg": {"field": "d", "format": "0.00"}}, "stats_fmt": {"stats": {"field": "n", "format": "#,##0.0"}},
        "es_t": {"extended_stats": {"field": "t"}},
    })
    add("metrics_filtered_query", {"avg_n": {"avg": {"field": "n"}}, "card_k": {"cardinality": {"field": "k"}}},
        query={"term": {"b": True}})
    # Terms.
    add("terms_keyword", {"t": {"terms": {"field": "k"}}})
    add("terms_keyword_size", {"t": {"terms": {"field": "k", "size": 3, "shard_size": 4, "show_term_doc_count_error": True}}})
    add("terms_keyword_order_key", {"t": {"terms": {"field": "k", "order": {"_key": "desc"}, "size": 5}}})
    add("terms_keyword_order_count_asc", {"t": {"terms": {"field": "k", "order": {"_count": "asc"}, "size": 4}}})
    add("terms_tag_high_card", {"t": {"terms": {"field": "tag", "size": 7}}})
    add("terms_long", {"t": {"terms": {"field": "n", "size": 6}}}, typed_keys=True)
    add("terms_double", {"t": {"terms": {"field": "d", "size": 3}}}, typed_keys=True)
    add("terms_bool_date_ip", {"b": {"terms": {"field": "b"}}, "ip": {"terms": {"field": "ip", "size": 4}},
                               "t": {"terms": {"field": "t", "size": 2, "order": {"_key": "asc"}}}}, typed_keys=True)
    add("terms_min_doc_count", {"t": {"terms": {"field": "k", "min_doc_count": 30}}})
    add("terms_min_doc_count_zero", {"t": {"terms": {"field": "k", "min_doc_count": 0, "size": 20}}}, query={"term": {"k": "k00"}})
    add("terms_include_regex", {"t": {"terms": {"field": "tag", "include": "t0[0-4].*", "exclude": "t00[0-5]", "size": 5}}})
    add("terms_include_values", {"t": {"terms": {"field": "k", "include": ["k01", "k03", "k07", "zzz"]}},
                                 "l": {"terms": {"field": "n", "exclude": [1, 2, 3, 4, 5], "size": 3}}})
    add("terms_partitions", {"p0": {"terms": {"field": "tag", "include": {"partition": 0, "num_partitions": 7}, "size": 4}},
                             "p1": {"terms": {"field": "n", "include": {"partition": 2, "num_partitions": 3}, "size": 4}}})
    add("terms_missing", {"t": {"terms": {"field": "k", "missing": "MISSING", "size": 4}},
                          "l": {"terms": {"field": "d", "missing": -1, "size": 2}}})
    add("terms_unmapped", {"t": {"terms": {"field": "nope"}}, "u": {"terms": {"field": "nope", "missing": "x"}}}, typed_keys=True)
    add("terms_sub_metric_order", {"t": {"terms": {"field": "k", "size": 5, "order": {"avg_d": "desc"}},
                                         "aggs": {"avg_d": {"avg": {"field": "d"}}}}})
    add("terms_sub_stats_order", {"t": {"terms": {"field": "k", "size": 5, "order": [{"st.max": "asc"}, {"_count": "desc"}]},
                                        "aggs": {"st": {"stats": {"field": "n"}}}}})
    add("terms_nested_terms", {"t": {"terms": {"field": "k", "size": 3},
                                     "aggs": {"inner": {"terms": {"field": "tag", "size": 2}, "aggs": {"m": {"max": {"field": "d"}}}}}}})
    add("terms_execution_map", {"t": {"terms": {"field": "k", "execution_hint": "map", "size": 4},
                                      "aggs": {"c": {"cardinality": {"field": "tag"}}}}})
    add("terms_filter_order_path", {"t": {"terms": {"field": "k", "size": 4, "order": {"f>m": "desc"}},
                                          "aggs": {"f": {"filter": {"term": {"b": True}}, "aggs": {"m": {"max": {"field": "n"}}}}}}})
    # Histograms.
    add("histogram_basic", {"h": {"histogram": {"field": "n", "interval": 10}}})
    add("histogram_offset_bounds", {"h": {"histogram": {"field": "d", "interval": 25, "offset": 5, "min_doc_count": 0,
                                                       "extended_bounds": {"min": -100, "max": 200}}}})
    add("histogram_keyed_order", {"h": {"histogram": {"field": "i", "interval": 7, "keyed": True, "order": {"_count": "desc"}, "min_doc_count": 1}},
                                  "h2": {"histogram": {"field": "f", "interval": 2.5, "format": "0.0", "order": {"_key": "desc"}}}})
    add("histogram_hard_bounds", {"h": {"histogram": {"field": "n", "interval": 5, "hard_bounds": {"min": 12, "max": 40}}}})
    add("histogram_subaggs", {"h": {"histogram": {"field": "n", "interval": 20}, "aggs": {"s": {"sum": {"field": "d"}}, "k": {"terms": {"field": "k", "size": 2}}}}})
    add("date_histogram_month", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}}}, typed_keys=True)
    add("date_histogram_tz_dst", {"h": {"date_histogram": {"field": "t", "calendar_interval": "day", "time_zone": "Europe/Paris"}}},
        query={"range": {"t": {"gte": 1585180800000, "lt": 1585785600000}}})
    add("date_histogram_tz_hour", {"h": {"date_histogram": {"field": "t", "calendar_interval": "hour", "time_zone": "America/New_York"}}},
        query={"range": {"t": {"gte": 1604098800000, "lt": 1604358000000}}})
    add("date_histogram_fixed", {"h": {"date_histogram": {"field": "t", "fixed_interval": "10d", "time_zone": "+05:30", "format": "yyyy-MM-dd HH:mm"}}})
    add("date_histogram_week_quarter_year", {"w": {"date_histogram": {"field": "t", "calendar_interval": "week", "min_doc_count": 3}},
                                             "q": {"date_histogram": {"field": "t", "calendar_interval": "quarter"}},
                                             "y": {"date_histogram": {"field": "t", "calendar_interval": "1y", "keyed": True}}})
    add("date_histogram_offset_bounds", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "offset": "+3d",
                                                                 "extended_bounds": {"min": "2019-06-01", "max": "2021-09-01"}}}})
    add("date_histogram_hard_bounds", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month",
                                                               "hard_bounds": {"min": "2020-03-01", "max": "2020-06-01"}}}})
    add("date_histogram_order_sub", {"h": {"date_histogram": {"field": "t", "calendar_interval": "quarter", "order": {"a": "asc"}},
                                           "aggs": {"a": {"avg": {"field": "n"}}}}})
    add("date_histogram_missing", {"h": {"date_histogram": {"field": "t", "calendar_interval": "year", "missing": "2015-05-05"}}})
    # Ranges.
    add("range_numeric", {"r": {"range": {"field": "d", "ranges": [{"to": 20}, {"from": 20, "to": 50}, {"from": 50}, {"key": "mid", "from": 40, "to": 60}]}},
                          "rk": {"range": {"field": "n", "keyed": True, "ranges": [{"to": 10}, {"from": 10, "to": 10}, {"from": 90}]}}}, typed_keys=True)
    add("range_float_precision", {"r": {"range": {"field": "f", "ranges": [{"to": 0.1}, {"from": 0.1, "to": 5.3}, {"from": 5.3}]}}})
    add("date_range", {"r": {"date_range": {"field": "t", "format": "yyyy-MM", "ranges": [{"to": "2020-06"}, {"from": "2020-06", "to": "2021-01"}, {"from": "2021-01"}]}},
                       "r2": {"date_range": {"field": "t", "time_zone": "Europe/Paris", "ranges": [{"from": "2020-03-29", "to": "2020-03-30"}]}}}, typed_keys=True)
    add("ip_range", {"r": {"ip_range": {"field": "ip", "ranges": [{"to": "10.0.0.10"}, {"from": "10.0.0.10", "to": "192.168.1.5"}, {"mask": "192.168.1.0/30"}, {"from": "2001:db8::4"}]}},
                     "rk": {"ip_range": {"field": "ip", "keyed": True, "ranges": [{"mask": "10.0.0.0/28", "key": "small"}, {"mask": "::/0"}]}}}, typed_keys=True)
    add("ip_prefix", {"p": {"ip_prefix": {"field": "ip", "prefix_length": 24}},
                      "p6": {"ip_prefix": {"field": "ip", "prefix_length": 64, "is_ipv6": True, "append_prefix_length": True, "keyed": True}}})
    add("geo_distance", {"g": {"geo_distance": {"field": "g", "origin": "10, 20", "unit": "km", "ranges": [{"to": 2000}, {"from": 2000, "to": 6000}, {"from": 6000}]}},
                         "gp": {"geo_distance": {"field": "g", "origin": [0, 0], "distance_type": "plane", "keyed": True, "ranges": [{"to": 3000000}, {"from": 3000000}]}}}, typed_keys=True)
    # Single bucket and filters.
    add("filter_filters", {"f": {"filter": {"range": {"n": {"gte": 50}}}, "aggs": {"a": {"avg": {"field": "n"}}}},
                           "fs": {"filters": {"filters": {"zz": {"term": {"k": "k01"}}, "aa": {"term": {"b": True}}}, "other_bucket_key": "rest"}},
                           "fa": {"filters": {"filters": [{"term": {"k": "k00"}}, {"range": {"d": {"lt": 30}}}], "other_bucket": True}},
                           "fk": {"filters": {"keyed": False, "filters": {"b": {"term": {"k": "k02"}}, "a": {"term": {"k": "k03"}}}}}}, typed_keys=True)
    add("adjacency_matrix", {"a": {"adjacency_matrix": {"filters": {"grpB": {"term": {"b": True}}, "grpA": {"term": {"k": "k00"}}, "grpC": {"range": {"n": {"lt": 30}}}}},
                                   "aggs": {"m": {"max": {"field": "d"}}}}}, typed_keys=True)
    add("missing_global", {"m": {"missing": {"field": "d"}, "aggs": {"c": {"value_count": {"field": "n"}}}}, "mk": {"missing": {"field": "k"}},
                           "g": {"global": {}, "aggs": {"a": {"avg": {"field": "n"}}}}}, query={"term": {"b": True}}, typed_keys=True)
    add("nested", {"n": {"nested": {"path": "nest"}, "aggs": {"x": {"terms": {"field": "nest.y"}, "aggs": {"r": {"reverse_nested": {}, "aggs": {"k": {"terms": {"field": "k", "size": 2}}}}}},
                                                          "s": {"stats": {"field": "nest.x"}}}}}, typed_keys=True)
    add("nested_under_terms", {"t": {"terms": {"field": "k", "size": 3}, "aggs": {"n": {"nested": {"path": "nest"}, "aggs": {"m": {"max": {"field": "nest.x"}}}}}}})
    add("sampler", {"s": {"sampler": {"shard_size": 50}, "aggs": {"k": {"terms": {"field": "k", "size": 3}}}},
                    "ds": {"diversified_sampler": {"field": "k", "shard_size": 20}, "aggs": {"c": {"value_count": {"field": "k"}}}}},
        query={"range": {"n": {"gte": 10}}})
    # Pipelines.
    add("pipelines_sibling", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"sum": {"field": "d"}}}},
                              "avg_b": {"avg_bucket": {"buckets_path": "h>s"}}, "max_b": {"max_bucket": {"buckets_path": "h>s"}},
                              "min_b": {"min_bucket": {"buckets_path": "h>_count"}}, "sum_b": {"sum_bucket": {"buckets_path": "h>s", "format": "0.0"}},
                              "st_b": {"stats_bucket": {"buckets_path": "h>s"}}, "es_b": {"extended_stats_bucket": {"buckets_path": "h>s", "sigma": 3}},
                              "p_b": {"percentiles_bucket": {"buckets_path": "h>s", "percents": [10, 50, 99]}},
                              "p_b2": {"percentiles_bucket": {"buckets_path": "h>s", "keyed": False}}}, typed_keys=True)
    add("pipelines_parent", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"},
                                   "aggs": {"s": {"sum": {"field": "n"}}, "c": {"cardinality": {"field": "k"}},
                                            "d": {"derivative": {"buckets_path": "s"}}, "d2": {"derivative": {"buckets_path": "d"}},
                                            "du": {"derivative": {"buckets_path": "s", "unit": "1d"}},
                                            "cs": {"cumulative_sum": {"buckets_path": "s"}}, "cc": {"cumulative_cardinality": {"buckets_path": "c"}},
                                            "sd": {"serial_diff": {"buckets_path": "s", "lag": 2}},
                                            "mf": {"moving_fn": {"buckets_path": "s", "window": 3, "script": "MovingFunctions.unweightedAvg(values)"}},
                                            "mf2": {"moving_fn": {"buckets_path": "_count", "window": 4, "shift": 1, "script": "MovingFunctions.linearWeightedAvg(values)"}},
                                            "nz": {"normalize": {"buckets_path": "s", "method": "percent_of_sum"}},
                                            "nz2": {"normalize": {"buckets_path": "s", "method": "z-score"}}}}}, typed_keys=True)
    add("pipelines_bucket_sort", {"t": {"terms": {"field": "k", "size": 10}, "aggs": {"a": {"avg": {"field": "n"}},
                                                                                       "srt": {"bucket_sort": {"sort": [{"a": {"order": "desc"}}], "size": 3, "from": 1}}}}})
    add("pipelines_sibling_in_bucket", {"t": {"terms": {"field": "b"}, "aggs": {"h": {"histogram": {"field": "n", "interval": 25}, "aggs": {"m": {"max": {"field": "d"}}}},
                                                                             "mb": {"max_bucket": {"buckets_path": "h>m"}}}}})
    # Percentiles.
    add("percentiles_basic", {
        "p": {"percentiles": {"field": "d"}}, "pk": {"percentiles": {"field": "n", "percents": [0, 10, 50, 99.9, 100], "keyed": False}},
        "pr": {"percentile_ranks": {"field": "d", "values": [10, 50, 75.5, 200]}}, "pt": {"percentiles": {"field": "t", "percents": [50]}},
        "pm": {"percentiles": {"field": "d", "missing": 0, "percents": [1]}}, "pu": {"percentiles": {"field": "nope"}},
        "pru": {"percentile_ranks": {"field": "nope", "values": [1]}}, "pf": {"percentiles": {"field": "f", "percents": ["25", 75], "format": "0.0"}},
    }, typed_keys=True)
    add("percentiles_merging", {
        "p": {"percentiles": {"field": "d", "tdigest": {"compression": 1}}},
        "p5": {"percentiles": {"field": "i", "tdigest": {"compression": 5}, "percents": [5, 50, 95]}},
        "pr": {"percentile_ranks": {"field": "d", "values": [30, 60], "tdigest": {"compression": 2}}},
    })
    add("percentiles_hdr", {
        "h": {"percentiles": {"field": "d", "hdr": {"number_of_significant_value_digits": 2}}},
        "h0": {"percentiles": {"field": "n", "hdr": {"number_of_significant_value_digits": 0}}},
        "hr": {"percentile_ranks": {"field": "n", "values": [5, 50, 95], "hdr": {}}}, "hu": {"percentiles": {"field": "nope", "hdr": {}}},
        "hk": {"percentiles": {"field": "f", "missing": 3, "keyed": False, "percents": [50], "hdr": {"number_of_significant_value_digits": 5}}},
    }, typed_keys=True)
    add("percentiles_in_buckets", {
        "t": {"terms": {"field": "k", "size": 4, "order": {"p.50": "desc"}},
              "aggs": {"p": {"percentiles": {"field": "d", "tdigest": {"compression": 3}}},
                       "h": {"percentiles": {"field": "n", "percents": [50], "hdr": {}}}}},
        "h": {"histogram": {"field": "n", "interval": 50},
              "aggs": {"p": {"percentile_ranks": {"field": "d", "values": [50], "tdigest": {"compression": 1}}}}},
        "mx": {"max_bucket": {"buckets_path": "h>p[50.0]"}},
    })
    add("percentiles_hdr_negative", {"h": {"percentiles": {"field": "i", "hdr": {}}}})
    add("percentiles_err_two_methods", {"p": {"percentiles": {"field": "d", "tdigest": {}, "hdr": {}}}})
    add("percentiles_err_range", {"p": {"percentiles": {"field": "d", "percents": [50, 101]}}})
    add("percentiles_err_dup", {"p": {"percentiles": {"field": "d", "percents": [50, 50.0]}}})
    add("percentiles_err_ranks_no_values", {"p": {"percentile_ranks": {"field": "d"}}})
    add("percentiles_err_keyword", {"p": {"percentiles": {"field": "k"}}})
    add("percentiles_err_compression", {"p": {"percentiles": {"field": "d", "tdigest": {"compression": -2}}}})
    add("percentiles_err_digits", {"p": {"percentiles": {"field": "d", "hdr": {"number_of_significant_value_digits": 6}}}})
    add("percentiles_err_order_unknown", {"t": {"terms": {"field": "k", "order": {"p.42": "desc"}}, "aggs": {"p": {"percentiles": {"field": "d"}}}}})
    # Sketch metrics.
    add("mad_boxplot", {
        "mad": {"median_absolute_deviation": {"field": "d"}}, "madc": {"median_absolute_deviation": {"field": "n", "compression": 2}},
        "madf": {"median_absolute_deviation": {"field": "f", "format": "0.00"}}, "madu": {"median_absolute_deviation": {"field": "nope"}},
        "bp": {"boxplot": {"field": "d"}}, "bpc": {"boxplot": {"field": "i", "compression": 3}}, "bpu": {"boxplot": {"field": "nope"}},
        "bpf": {"boxplot": {"field": "n", "format": "0.0"}},
    }, typed_keys=True)
    add("mad_boxplot_buckets", {
        "t": {"terms": {"field": "k", "size": 4, "order": {"bp.q3": "asc"}},
              "aggs": {"bp": {"boxplot": {"field": "d", "compression": 2}}, "mad": {"median_absolute_deviation": {"field": "d", "compression": 3}}}},
        "t2": {"terms": {"field": "k", "size": 3, "order": {"mad": "desc"}}, "aggs": {"mad": {"median_absolute_deviation": {"field": "n", "compression": 2}}}},
        "h": {"histogram": {"field": "n", "interval": 40}, "aggs": {"bp": {"boxplot": {"field": "i"}}}},
    })
    add("mad_err_compression", {"m": {"median_absolute_deviation": {"field": "d", "compression": 0}}})
    add("boxplot_err_metric", {"t": {"terms": {"field": "k", "order": {"bp.median": "asc"}}, "aggs": {"bp": {"boxplot": {"field": "d"}}}}})
    # Top hits (no query: every score is 1.0).
    add("top_hits_basic", {
        "t": {"top_hits": {"size": 2}},
        "ts": {"top_hits": {"size": 3, "sort": [{"d": "desc"}], "_source": ["d", "k"]}},
        "tf": {"top_hits": {"from": 2, "size": 2, "sort": [{"k": {"order": "asc"}}, {"n": {"order": "desc", "mode": "min"}}], "_source": False}},
        "tm": {"top_hits": {"size": 2, "sort": [{"d": {"order": "asc", "missing": "_first"}}], "_source": {"includes": ["d"]}}},
        "tml": {"top_hits": {"size": 2, "sort": [{"d": {"order": "desc", "missing": "_last"}}, {"i": "asc"}], "_source": ["d", "i"], "from": 359}},
        "tsc": {"top_hits": {"size": 1, "sort": ["_score", {"t": "desc"}], "track_scores": True, "_source": ["t"]}},
        "tip": {"top_hits": {"size": 2, "sort": [{"ip": "desc"}, {"f": "asc"}], "_source": ["ip", "f"]}},
    }, typed_keys=True)
    add("top_hits_in_terms", {"t": {"terms": {"field": "k", "size": 3},
                                    "aggs": {"top": {"top_hits": {"size": 1, "sort": [{"t": {"order": "desc"}}], "_source": ["t", "k"]}}}},
                              "h": {"histogram": {"field": "n", "interval": 50}, "aggs": {"top": {"top_hits": {"size": 2, "_source": ["n"]}}}}})
    add("top_hits_err_window", {"t": {"top_hits": {"size": 101}}})
    add("top_hits_err_key", {"t": {"top_hits": {"size": 1, "bogus": 3}}})
    add("top_hits_err_order_by", {"t": {"terms": {"field": "k", "order": {"top": "asc"}}, "aggs": {"top": {"top_hits": {}}}}})
    # Composite.
    add("composite_basic", {"c": {"composite": {"size": 5, "sources": [{"k": {"terms": {"field": "k"}}}, {"n": {"histogram": {"field": "n", "interval": 25}}}]},
                                  "aggs": {"a": {"avg": {"field": "d"}}}}}, typed_keys=True)
    add("composite_after", {"c": {"composite": {"size": 4, "after": {"k": "k02", "n": 50},
                                                "sources": [{"k": {"terms": {"field": "k", "order": "desc"}}}, {"n": {"histogram": {"field": "n", "interval": 25}}}]}}})
    add("composite_missing", {"c": {"composite": {"size": 6, "sources": [{"k": {"terms": {"field": "k", "missing_bucket": True}}}, {"b": {"terms": {"field": "b"}}}]}},
                              "c2": {"composite": {"size": 3, "sources": [{"d": {"terms": {"field": "d", "missing_bucket": True, "missing_order": "last", "order": "desc"}}}]}},
                              "c3": {"composite": {"size": 2, "after": {"d": None}, "sources": [{"d": {"terms": {"field": "d", "missing_bucket": True}}}]}}})
    add("composite_dates", {"c": {"composite": {"size": 4, "sources": [{"m": {"date_histogram": {"field": "t", "calendar_interval": "month", "format": "yyyy-MM", "time_zone": "Europe/Paris"}}},
                                                                     {"ip": {"terms": {"field": "ip"}}}]}},
                            "c2": {"composite": {"size": 3, "sources": [{"t": {"terms": {"field": "t"}}}], "after": {"t": 1580000000000}}},
                            "c3": {"composite": {"size": 3, "sources": [{"d": {"date_histogram": {"field": "t", "fixed_interval": "7d", "offset": "+1d"}}}]}},
                            "c4": {"composite": {"size": 2, "sources": [{"h": {"histogram": {"field": "t", "interval": 86400000}}}], "after": {"h": "2020-03-01T00:00:00.000Z"}}}})
    add("composite_geotile", {"c": {"composite": {"size": 5, "sources": [{"g": {"geotile_grid": {"field": "g", "precision": 3}}}]}},
                              "c2": {"composite": {"size": 4, "sources": [{"g": {"geotile_grid": {"field": "g", "precision": 8,
                                                                                                   "bounds": {"top_left": "50, -100", "bottom_right": "-20, 30"}}}}]}}})
    add("composite_under_filter", {"f": {"filter": {"term": {"b": True}},
                                         "aggs": {"c": {"composite": {"size": 3, "sources": [{"x": {"terms": {"field": "i"}}}]}, "aggs": {"m": {"max": {"field": "f"}}}}}}})
    add("composite_pipeline", {"c": {"composite": {"size": 4, "sources": [{"k": {"terms": {"field": "k"}}}]},
                                     "aggs": {"s": {"sum": {"field": "n"}}, "bs": {"bucket_sort": {"sort": [{"s": "desc"}]}}}}})
    add("composite_err_parent", {"t": {"terms": {"field": "k"}, "aggs": {"c": {"composite": {"sources": [{"a": {"terms": {"field": "k"}}}]}}}}})
    add("composite_err_dup", {"c": {"composite": {"sources": [{"a": {"terms": {"field": "k"}}}, {"a": {"terms": {"field": "n"}}}]}}})
    add("composite_err_after", {"c": {"composite": {"sources": [{"a": {"terms": {"field": "k"}}}], "after": {"b": 1}}}})
    add("composite_err_missing_order", {"c": {"composite": {"sources": [{"a": {"terms": {"field": "k", "missing_order": "first"}}}]}}})
    add("composite_err_no_sources", {"c": {"composite": {"size": 2}}})
    add("composite_err_source_type", {"c": {"composite": {"sources": [{"a": {"bogus": {"field": "k"}}}]}}})
    add("composite_err_size0", {"c": {"composite": {"size": 0, "sources": [{"a": {"terms": {"field": "k"}}}]}}})
    # Geo.
    add("geo_metrics", {"gb": {"geo_bounds": {"field": "g"}}, "gbw": {"geo_bounds": {"field": "g", "wrap_longitude": False}},
                        "gc": {"geo_centroid": {"field": "g"}}, "gbu": {"geo_bounds": {"field": "nope"}}, "gcu": {"geo_centroid": {"field": "nope"}},
                        "gcm": {"geo_centroid": {"field": "g", "missing": "10, 20"}}}, typed_keys=True)
    add("geo_metrics_in_buckets", {"t": {"terms": {"field": "k", "size": 3}, "aggs": {"gb": {"geo_bounds": {"field": "g"}}, "gc": {"geo_centroid": {"field": "g"}}}}})
    add("geo_grids", {"gh": {"geohash_grid": {"field": "g", "precision": 2, "size": 6}},
                      "gt": {"geotile_grid": {"field": "g", "precision": 4, "size": 5, "shard_size": 7}},
                      "ghd": {"geohash_grid": {"field": "g", "precision": "2000km", "size": 3}},
                      "ghb": {"geohash_grid": {"field": "g", "precision": 3, "bounds": {"top_left": "40, -100", "bottom_right": "-10, 50"}}},
                      "gtb": {"geotile_grid": {"field": "g", "precision": 5, "bounds": {"top_left": "40, -100", "bottom_right": "-10, 50"}, "size": 4}}},
        typed_keys=True)
    add("geo_grid_subaggs", {"gt": {"geotile_grid": {"field": "g", "precision": 2}, "aggs": {"c": {"geo_centroid": {"field": "g"}}, "a": {"avg": {"field": "n"}}}}})
    add("geo_grid_err_precision", {"g": {"geohash_grid": {"field": "g", "precision": 13}}})
    add("geo_grid_err_distance", {"g": {"geohash_grid": {"field": "g", "precision": "1cm"}}})
    add("geo_grid_err_type", {"g": {"geotile_grid": {"field": "k"}}})
    # Multi terms.
    add("multi_terms", {
        "mt": {"multi_terms": {"terms": [{"field": "k"}, {"field": "b"}], "size": 5}},
        "mt2": {"multi_terms": {"terms": [{"field": "n"}, {"field": "ip"}, {"field": "t", "format": "yyyy"}], "size": 4, "order": {"_key": "desc"}}},
        "mt3": {"multi_terms": {"terms": [{"field": "d", "missing": 0}, {"field": "tag"}], "size": 3, "show_term_doc_count_error": True, "shard_size": 5},
                "aggs": {"a": {"avg": {"field": "n"}}}},
        "mt4": {"multi_terms": {"terms": [{"field": "k"}, {"field": "nope"}]}},
        "mt5": {"multi_terms": {"terms": [{"field": "k"}, {"field": "n"}], "size": 3, "order": {"m": "desc"}, "min_doc_count": 2},
                "aggs": {"m": {"max": {"field": "d"}}}},
    }, typed_keys=True)
    add("multi_terms_err_one", {"mt": {"multi_terms": {"terms": [{"field": "k"}]}}})
    add("multi_terms_err_none", {"mt": {"multi_terms": {"size": 3}}})
    add("multi_terms_err_both", {"mt": {"multi_terms": {"terms": [{"field": "k", "script": "1"}, {"field": "n"}]}}})
    # Rare terms.
    add("rare_terms", {
        "r": {"rare_terms": {"field": "tag"}}, "r2": {"rare_terms": {"field": "n", "max_doc_count": 3}},
        "r3": {"rare_terms": {"field": "tag", "max_doc_count": 2, "include": "t0.*", "exclude": ["t001"]}, "aggs": {"m": {"max": {"field": "n"}}}},
        "r4": {"rare_terms": {"field": "nope"}}, "r5": {"rare_terms": {"field": "ip", "max_doc_count": 5}},
        "r6": {"rare_terms": {"field": "t", "max_doc_count": 1}},
    }, typed_keys=True)
    add("rare_terms_in_buckets", {"t": {"terms": {"field": "b"}, "aggs": {"r": {"rare_terms": {"field": "k", "max_doc_count": 4}}}}})
    add("rare_terms_err_double", {"r": {"rare_terms": {"field": "d"}}})
    add("rare_terms_err_max", {"r": {"rare_terms": {"field": "k", "max_doc_count": 101}}})
    add("rare_terms_err_zero", {"r": {"rare_terms": {"field": "k", "max_doc_count": 0}}})
    # Significant terms.
    add("significant_terms", {
        "s": {"significant_terms": {"field": "k", "min_doc_count": 2}},
        "s2": {"significant_terms": {"field": "tag", "min_doc_count": 1, "size": 5, "percentage": {}}},
        "s3": {"significant_terms": {"field": "n", "min_doc_count": 1, "size": 4, "chi_square": {"include_negatives": True}}},
        "s4": {"significant_terms": {"field": "k", "min_doc_count": 1, "mutual_information": {}}},
        "s5": {"significant_terms": {"field": "k", "min_doc_count": 1, "gnd": {}}},
        "s6": {"significant_terms": {"field": "ip", "min_doc_count": 1, "size": 3}},
        "s7": {"significant_terms": {"field": "nope"}},
        "s8": {"significant_terms": {"field": "tag", "min_doc_count": 1, "size": 4, "include": "t1.*"}},
    }, query={"term": {"b": True}}, typed_keys=True)
    add("significant_terms_bg", {"s": {"significant_terms": {"field": "k", "min_doc_count": 1, "background_filter": {"range": {"n": {"lt": 60}}},
                                                             "chi_square": {"background_is_superset": False}}},
                                 "s2": {"significant_terms": {"field": "k", "min_doc_count": 1, "background_filter": {"range": {"n": {"lt": 60}}}}}},
        query={"range": {"n": {"lt": 30}}})
    add("significant_terms_in_terms", {"t": {"terms": {"field": "b"}, "aggs": {"s": {"significant_terms": {"field": "k", "min_doc_count": 1, "size": 3},
                                                                                     "aggs": {"m": {"max": {"field": "n"}}}}}}})
    add("significant_terms_err_double", {"s": {"significant_terms": {"field": "d"}}})
    add("significant_terms_err_heuristic", {"s": {"significant_terms": {"field": "k", "bogus": {}}}})
    add("significant_terms_err_superset", {"s": {"significant_terms": {"field": "k", "min_doc_count": 1, "background_filter": {"term": {"k": "k00"}},
                                                                       "chi_square": {}}}})
    # Significant text (standard analyzer).
    add("significant_text", {
        "s": {"significant_text": {"field": "txt", "min_doc_count": 2}},
        "s2": {"significant_text": {"field": "txt", "min_doc_count": 1, "filter_duplicate_text": True, "size": 5}},
        "s3": {"significant_text": {"field": "txt", "min_doc_count": 1, "percentage": {}, "exclude": ["the"]}},
        "s4": {"significant_text": {"field": "nope"}},
    }, query={"term": {"b": True}}, typed_keys=True)
    add("significant_text_err_subs", {"s": {"significant_text": {"field": "txt"}, "aggs": {"m": {"max": {"field": "n"}}}}})
    add("significant_text_err_numeric", {"s": {"significant_text": {"field": "n"}}})
    # String stats.
    add("string_stats", {"s": {"string_stats": {"field": "k"}}, "s2": {"string_stats": {"field": "tag", "show_distribution": True}},
                         "s3": {"string_stats": {"field": "k", "show_distribution": True, "missing": "x\u00e9\U0001F600"}}, "s4": {"string_stats": {"field": "nope"}},
                         "s5": {"string_stats": {"field": "nope", "show_distribution": True}}}, typed_keys=True)
    add("string_stats_in_buckets", {"t": {"terms": {"field": "b"}, "aggs": {"s": {"string_stats": {"field": "tag", "show_distribution": True}}}}})
    add("string_stats_err_numeric", {"s": {"string_stats": {"field": "n"}}})
    add("string_stats_err_ip", {"s": {"string_stats": {"field": "ip"}}})
    # Top metrics.
    add("top_metrics", {
        "tm": {"top_metrics": {"metrics": [{"field": "n"}, {"field": "d"}, {"field": "k"}, {"field": "t"}, {"field": "b"}, {"field": "ip"}], "sort": {"t": "desc"}, "size": 2}},
        "tm2": {"top_metrics": {"metrics": {"field": "d"}, "sort": "_score", "size": 3}},
        "tm3": {"top_metrics": {"metrics": {"field": "n"}, "sort": {"d": "asc"}, "size": 3}},
        "tm4": {"top_metrics": {"metrics": [{"field": "nope"}, {"field": "f"}], "sort": {"n": {"order": "desc", "mode": "min"}}, "size": 4}},
        "tm5": {"top_metrics": {"metrics": {"field": "i", "missing": 7}, "sort": {"f": "asc"}}},
    }, typed_keys=True)
    add("top_metrics_in_buckets", {"t": {"terms": {"field": "k", "size": 4, "order": {"tm.n": "desc"}},
                                         "aggs": {"tm": {"top_metrics": {"metrics": {"field": "n"}, "sort": {"t": "asc"}}}}},
                                   "h": {"histogram": {"field": "n", "interval": 50}, "aggs": {"tm": {"top_metrics": {"metrics": {"field": "d"}, "sort": {"i": "desc"}, "size": 2}}}}})
    add("top_metrics_err_two_sorts", {"tm": {"top_metrics": {"metrics": {"field": "n"}, "sort": [{"n": "asc"}, {"d": "asc"}]}}})
    add("top_metrics_err_size", {"tm": {"top_metrics": {"metrics": {"field": "n"}, "sort": {"n": "asc"}, "size": 11}}})
    add("top_metrics_err_keyword_sort", {"tm": {"top_metrics": {"metrics": {"field": "n"}, "sort": {"tag": "asc"}}}})
    add("top_metrics_err_geo", {"tm": {"top_metrics": {"metrics": {"field": "g"}, "sort": {"n": "asc"}}}})
    add("top_metrics_err_required", {"tm": {"top_metrics": {"sort": {"n": "asc"}}}})
    # Rate.
    add("rate_calendar", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {
        "r_day": {"rate": {"unit": "day", "field": "n"}}, "r_docs": {"rate": {"unit": "month"}}, "r_none": {"rate": {"field": "d"}},
        "r_vc": {"rate": {"unit": "year", "field": "n", "mode": "value_count"}}, "r_q": {"rate": {"unit": "quarter", "field": "i"}},
        "r_hour": {"rate": {"unit": "hour"}}, "r_un": {"rate": {"unit": "day", "field": "nope"}},
        "r_unm": {"rate": {"unit": "day", "field": "nope", "missing": 2}}, "r_fmt": {"rate": {"unit": "week", "field": "d", "format": "0.00"}},
        "f": {"filter": {"term": {"b": True}}, "aggs": {"r": {"rate": {"unit": "month", "field": "n"}}, "r2": {"rate": {"unit": "year"}}}}}},
        "mb": {"max_bucket": {"buckets_path": "h>r_day"}}, "mx": {"max_bucket": {"buckets_path": "h>f>r"}}}, typed_keys=True)
    add("rate_fixed_and_tz", {
        "h": {"date_histogram": {"field": "t", "fixed_interval": "10d"}, "aggs": {"r": {"rate": {"unit": "day", "field": "d"}}, "r2": {"rate": {"unit": "hour"}}, "r3": {"rate": {"field": "n"}}}},
        "w": {"date_histogram": {"field": "t", "calendar_interval": "week"}, "aggs": {"r": {"rate": {"unit": "day"}}}},
        "z": {"date_histogram": {"field": "t", "calendar_interval": "month", "time_zone": "Europe/Berlin"}, "aggs": {"r": {"rate": {"unit": "hour", "field": "n"}}}},
        "o": {"date_histogram": {"field": "t", "calendar_interval": "month", "offset": "+1d"}, "aggs": {"r": {"rate": {"unit": "day", "field": "n"}}}},
        "y": {"date_histogram": {"field": "t", "calendar_interval": "year"}, "aggs": {"k": {"terms": {"field": "k", "size": 3, "order": {"r": "desc"}}, "aggs": {"r": {"rate": {"unit": "month", "field": "n"}}}}}},
        "e": {"date_histogram": {"field": "t", "calendar_interval": "quarter", "min_doc_count": 0, "extended_bounds": {"min": "2019-01-01", "max": "2021-12-31"}}, "aggs": {"r": {"rate": {"unit": "month", "field": "n"}}}},
    })
    add("rate_composite", {"c": {"composite": {"size": 6, "sources": [{"m": {"date_histogram": {"field": "t", "calendar_interval": "month"}}}, {"b": {"terms": {"field": "b"}}}]},
                                 "aggs": {"r": {"rate": {"unit": "day", "field": "n"}}, "r2": {"rate": {"unit": "month"}}}}})
    add("rate_in_terms_in_histo", {"h": {"date_histogram": {"field": "t", "calendar_interval": "quarter"}, "aggs": {"t": {"terms": {"field": "b"}, "aggs": {"r": {"rate": {"unit": "month"}}}}}}})
    add("rate_err_top_level", {"r": {"rate": {"unit": "day"}}})
    add("rate_err_in_histogram", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"r": {"rate": {"unit": "day"}}}}})
    add("rate_err_unit", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"r": {"rate": {"unit": "fortnight"}}}}})
    add("rate_err_mode_value", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"r": {"rate": {"field": "n", "mode": "avg"}}}}})
    add("rate_err_mode_without_field", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"r": {"rate": {"mode": "sum"}}}}})
    add("rate_err_keyword", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"r": {"rate": {"field": "k"}}}}})
    add("rate_err_month_on_fixed", {"h": {"date_histogram": {"field": "t", "fixed_interval": "7d"}, "aggs": {"r": {"rate": {"unit": "month"}}}}})
    add("rate_err_month_on_week", {"h": {"date_histogram": {"field": "t", "calendar_interval": "week"}, "aggs": {"r": {"rate": {"unit": "month"}}}}})
    add("rate_err_day_nested_month", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"f": {"filter": {"term": {"b": True}}, "aggs": {"r": {"rate": {"unit": "day"}}}}}}})
    add("rate_err_composite_no_date", {"c": {"composite": {"sources": [{"k": {"terms": {"field": "k"}}}]}, "aggs": {"r": {"rate": {"unit": "day"}}}}})
    add("rate_err_sub_aggs", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"r": {"rate": {"unit": "day"}, "aggs": {"m": {"max": {"field": "n"}}}}}}})
    # Matrix stats.
    add("matrix_stats", {
        "m": {"matrix_stats": {"fields": ["n", "d", "i", "f"]}},
        "m_min": {"matrix_stats": {"fields": ["d", "n"], "mode": "min"}},
        "m_max": {"matrix_stats": {"fields": ["n", "i"], "mode": "MAX"}},
        "m_sum": {"matrix_stats": {"fields": ["n", "f"], "mode": "sum"}},
        "m_med": {"matrix_stats": {"fields": ["n", "d"], "mode": "median"}},
        "m_one": {"matrix_stats": {"fields": "d"}},
        "m_miss": {"matrix_stats": {"fields": ["d", "n", "b", "t"], "missing": {"d": 0, "n": "7"}}},
        "m_unmapped": {"matrix_stats": {"fields": ["nope", "i", "f"]}},
        "m_none": {"matrix_stats": {"fields": ["nope"]}},
        "m_dup": {"matrix_stats": {"fields": ["i", "f", "i"]}},
    }, typed_keys=True)
    add("matrix_stats_in_buckets", {"t": {"terms": {"field": "k", "size": 3}, "aggs": {"m": {"matrix_stats": {"fields": ["i", "d", "n"]}}}},
                                    "f": {"filter": {"term": {"i": 3}}, "aggs": {"m": {"matrix_stats": {"fields": ["i", "d"]}}}}})
    add("matrix_stats_many_fields", {"m": {"matrix_stats": {"fields": ["n", "d", "i", "f", "b", "t", "nest.x"], "missing": {"nest.x": 1}}}})
    add("matrix_stats_err_mode", {"m": {"matrix_stats": {"fields": ["n"], "mode": "foo"}}})
    add("matrix_stats_err_script", {"m": {"matrix_stats": {"fields": ["n"], "script": {"source": "1"}}}})
    add("matrix_stats_err_value_type", {"m": {"matrix_stats": {"fields": "n", "value_type": "long"}}})
    add("matrix_stats_err_token", {"m": {"matrix_stats": {"fields": ["n"], "bad": 1}}})
    add("matrix_stats_err_fields_item", {"m": {"matrix_stats": {"fields": ["n", 3]}}})
    add("matrix_stats_err_sub_aggs", {"m": {"matrix_stats": {"fields": ["n"]}, "aggs": {"x": {"max": {"field": "n"}}}}})
    # Scripted metric (errors only: there is no script engine in the harness).
    add("scripted_metric_err_map", {"m": {"scripted_metric": {"init_script": "state.x = 0"}}})
    add("scripted_metric_err_combine", {"m": {"scripted_metric": {"map_script": "state.x = 1"}}})
    add("scripted_metric_err_reduce", {"m": {"scripted_metric": {"map_script": "state.x = 1", "combine_script": "return 1"}}})
    add("scripted_metric_err_field", {"m": {"scripted_metric": {"map_script": "state.x = 1", "combine_script": "return 1", "reduce_script": "return 1", "foo": 1}}})
    add("scripted_metric_err_params", {"m": {"scripted_metric": {"map_script": "state.x = 1", "combine_script": "return 1", "reduce_script": "return 1", "params": "x"}}})
    add("scripted_metric_err_param_conflict", {"m": {"scripted_metric": {"map_script": {"source": "state.x = 1", "params": {"a": 1}}, "combine_script": "return 1", "reduce_script": "return 1", "params": {"a": 2}}}})
    # Auto date histogram.
    add("auto_date_histogram", {
        "a": {"auto_date_histogram": {"field": "t"}},
        "a3": {"auto_date_histogram": {"field": "t", "buckets": 3}, "aggs": {"s": {"sum": {"field": "n"}}, "k": {"terms": {"field": "k", "size": 2}}}},
        "a20": {"auto_date_histogram": {"field": "t", "buckets": 20, "format": "yyyy-MM-dd"}},
        "a50": {"auto_date_histogram": {"field": "t", "buckets": 50, "time_zone": "America/New_York"}},
        "a100": {"auto_date_histogram": {"field": "t", "buckets": 100}, "aggs": {"m": {"max": {"field": "d"}}}},
        "a1": {"auto_date_histogram": {"field": "t", "buckets": 1}},
        "amin": {"auto_date_histogram": {"field": "t", "buckets": 400, "minimum_interval": "month"}},
        "amiss": {"auto_date_histogram": {"field": "t", "missing": "2018-06-01", "buckets": 7}},
        "aun": {"auto_date_histogram": {"field": "nope"}},
        "anum": {"auto_date_histogram": {"field": "n", "buckets": 4}},
    }, typed_keys=True)
    add("auto_date_histogram_narrow", {
        "a": {"auto_date_histogram": {"field": "t", "buckets": 6}, "aggs": {"c": {"value_count": {"field": "n"}}}},
        "b": {"auto_date_histogram": {"field": "t", "buckets": 40}},
        "c": {"auto_date_histogram": {"field": "t", "buckets": 200, "minimum_interval": "minute"}},
    }, query={"range": {"t": {"gte": 1590969600000, "lt": 1592697600000}}})
    add("auto_date_histogram_hours", {
        "a": {"auto_date_histogram": {"field": "t", "buckets": 5}},
        "b": {"auto_date_histogram": {"field": "t", "buckets": 30}},
    }, query={"range": {"t": {"gte": 1590969600000, "lt": 1591315200000}}})
    add("auto_date_histogram_under_terms", {"k": {"terms": {"field": "k", "size": 4}, "aggs": {
        "a": {"auto_date_histogram": {"field": "t", "buckets": 5}, "aggs": {"m": {"min": {"field": "i"}}}},
        "b": {"auto_date_histogram": {"field": "t", "buckets": 30}}}},
        "f": {"filter": {"term": {"b": True}}, "aggs": {"a": {"auto_date_histogram": {"field": "t", "buckets": 8}}}}})
    add("auto_date_histogram_pipelines", {"a": {"auto_date_histogram": {"field": "t", "buckets": 12}, "aggs": {
        "s": {"sum": {"field": "n"}}, "d": {"derivative": {"buckets_path": "s"}}, "c": {"cumulative_sum": {"buckets_path": "_count"}}}},
        "mx": {"max_bucket": {"buckets_path": "a>s"}}})
    add("auto_date_histogram_err_buckets", {"a": {"auto_date_histogram": {"field": "t", "buckets": 0}}})
    add("auto_date_histogram_err_ceiling", {"a": {"auto_date_histogram": {"field": "t", "buckets": 3000}}})
    add("auto_date_histogram_err_interval", {"a": {"auto_date_histogram": {"field": "t", "minimum_interval": "week"}}})
    add("auto_date_histogram_err_year", {"a": {"auto_date_histogram": {"field": "t", "minimum_interval": "year"}}})
    add("auto_date_histogram_err_keyword", {"a": {"auto_date_histogram": {"field": "k"}}})
    # Variable width histogram.
    add("variable_width_histogram", {
        "v": {"variable_width_histogram": {"field": "n", "buckets": 5}},
        "vd": {"variable_width_histogram": {"field": "d"}},
        "vf": {"variable_width_histogram": {"field": "f", "buckets": 4, "format": "0.0"}},
        "vi": {"variable_width_histogram": {"field": "i", "buckets": 5, "shard_size": 8, "initial_buffer": 20},
               "aggs": {"a": {"avg": {"field": "d"}}, "k": {"terms": {"field": "k", "size": 2}}}},
        "vm": {"variable_width_histogram": {"field": "d", "buckets": 3, "shard_size": 6, "initial_buffer": 7, "missing": 500}},
        "vu": {"variable_width_histogram": {"field": "nope"}},
        "vb": {"variable_width_histogram": {"field": "n", "buckets": 40, "initial_buffer": 50}},
    }, typed_keys=True)
    add("variable_width_histogram_in_filter", {"f": {"filter": {"term": {"b": True}}, "aggs": {
        "v": {"variable_width_histogram": {"field": "i", "buckets": 3, "shard_size": 4, "initial_buffer": 10}, "aggs": {"c": {"value_count": {"field": "n"}}}}}}})
    add("variable_width_histogram_unmapped_under_terms", {"t": {"terms": {"field": "b"}, "aggs": {"v": {"variable_width_histogram": {"field": "nope"}}}}})
    add("variable_width_histogram_err_under_terms", {"t": {"terms": {"field": "b"}, "aggs": {"v": {"variable_width_histogram": {"field": "n"}}}}})
    add("variable_width_histogram_err_buckets", {"v": {"variable_width_histogram": {"field": "n", "buckets": 0}}})
    add("variable_width_histogram_err_shard_size", {"v": {"variable_width_histogram": {"field": "n", "shard_size": 1}}})
    add("variable_width_histogram_err_buffer", {"v": {"variable_width_histogram": {"field": "n", "buckets": 20, "initial_buffer": 10}}})
    add("variable_width_histogram_err_merge", {"v": {"variable_width_histogram": {"field": "n", "buckets": 20, "shard_size": 20}}})
    add("variable_width_histogram_err_date", {"v": {"variable_width_histogram": {"field": "t"}}})
    add("variable_width_histogram_err_keyword", {"v": {"variable_width_histogram": {"field": "k"}}})
    tt = {"a": {"field": "d"}, "b": {"field": "f"}}
    add("t_test", {
        "welch": {"t_test": tt}, "homo": {"t_test": dict(tt, type="homoscedastic", tails=1)},
        "paired": {"t_test": {"a": {"field": "i"}, "b": {"field": "f"}, "type": "paired"}},
        "paired_missing": {"t_test": {"a": {"field": "d", "missing": 0}, "b": {"field": "i"}, "type": "PAIRED"}},
        "filtered": {"t_test": {"a": {"field": "d", "filter": {"term": {"k": "k01"}}}, "b": {"field": "d", "filter": {"term": {"k": "k02"}}}}},
        "one_filter": {"t_test": {"a": {"field": "i", "filter": {"term": {"b": True}}}, "b": {"field": "i"}, "type": "homoscedastic"}},
        "fmt": {"t_test": dict(tt, format="0.0000")},
        "multi": {"t_test": {"a": {"field": "n"}, "b": {"field": "t"}}},
        "unmapped": {"t_test": {"a": {"field": "nope"}, "b": {"field": "nope2"}}},
        "half_unmapped": {"t_test": {"a": {"field": "nope"}, "b": {"field": "d"}}},
        "unmapped_missing": {"t_test": {"a": {"field": "nope", "missing": 3}, "b": {"field": "d"}}},
    }, typed_keys=True)
    add("t_test_err_unmapped_paired_filters", {"t": {"t_test": {"a": {"field": "nope", "filter": {"term": {"k": "k01"}}}, "b": {"field": "nope2"}, "type": "paired"}}})
    add("t_test_under_terms", {"k": {"terms": {"field": "k", "order": {"t": "asc"}, "size": 6}, "aggs": {"t": {"t_test": tt}, "p": {"t_test": {"a": {"field": "i"}, "b": {"field": "d"}, "type": "paired"}}}},
                               "mx": {"min_bucket": {"buckets_path": "k>t"}}, "h": {"histogram": {"field": "i", "interval": 25}, "aggs": {"t": {"t_test": tt}}}})
    add("t_test_query", {"t": {"t_test": tt}}, query={"range": {"n": {"gte": 50}}})
    add("t_test_err_same_field", {"t": {"t_test": {"a": {"field": "d"}, "b": {"field": "d"}}}})
    add("t_test_err_type", {"t": {"t_test": dict(tt, type="foo")}})
    add("t_test_err_tails", {"t": {"t_test": dict(tt, tails=3)}})
    add("t_test_err_multi_paired", {"t": {"t_test": {"a": {"field": "n"}, "b": {"field": "d"}, "type": "paired"}}})
    add("t_test_err_paired_filters", {"t": {"t_test": {"a": {"field": "d", "filter": {"term": {"k": "k01"}}}, "b": {"field": "f"}, "type": "paired"}}})
    add("t_test_err_keyword", {"t": {"t_test": {"a": {"field": "d"}, "b": {"field": "k"}}}})
    add("t_test_err_missing_b", {"t": {"t_test": {"a": {"field": "d"}}}})
    add("t_test_err_value_type", {"t": {"t_test": dict(tt, value_type="string")}})
    add("t_test_err_field_config", {"t": {"t_test": {"a": {"field": "d", "foo": 1}, "b": {"field": "f"}}}})
    add("t_test_err_both", {"t": {"t_test": {"a": {"field": "d", "script": "1"}, "b": {"field": "f"}}}})
    add("t_test_err_subs", {"t": {"t_test": tt, "aggs": {"m": {"max": {"field": "d"}}}}})
    add("time_series_not_tsdb", {"ts": {"time_series": {}}, "tk": {"time_series": {"keyed": False}, "aggs": {"m": {"max": {"field": "n"}}}}})
    add("variable_width_histogram_nested", {"n": {"nested": {"path": "nest"}, "aggs": {"v": {"variable_width_histogram": {"field": "nest.x", "buckets": 3}, "aggs": {"y": {"terms": {"field": "nest.y"}}}}}}})
    add("variable_width_histogram_err_nested_scores", {"n": {"nested": {"path": "nest"}, "aggs": {"v": {"variable_width_histogram": {"field": "nest.x", "buckets": 3}, "aggs": {
        "m": {"max": {"field": "nest.x"}}, "th": {"top_hits": {"size": 1}}, "th2": {"top_hits": {"size": 2}}}}}}})
    add("variable_width_histogram_err_reverse_nested_scores", {"n": {"nested": {"path": "nest"}, "aggs": {"r": {"reverse_nested": {}, "aggs": {"v": {"variable_width_histogram": {"field": "n", "buckets": 3}, "aggs": {
        "th": {"top_hits": {"size": 1}}}}}}}}})
    # Random sampler (segment-layout sensitive: the sampler restarts its random stream per segment).
    add("random_sampler", {"rs": {"random_sampler": {"probability": 0.1, "seed": 42, "shard_seed": 7}, "aggs": {
        "avg": {"avg": {"field": "n"}}, "sum": {"sum": {"field": "n"}}, "vc": {"value_count": {"field": "n"}},
        "st": {"stats": {"field": "d"}}, "es": {"extended_stats": {"field": "d"}}, "mn": {"min": {"field": "i"}}, "mx": {"max": {"field": "f"}},
        "card": {"cardinality": {"field": "k"}}, "pct": {"percentiles": {"field": "d", "percents": [50, 90]}},
        "mad": {"median_absolute_deviation": {"field": "d"}}, "wavg": {"weighted_avg": {"value": {"field": "d"}, "weight": {"field": "i", "missing": 1}}},
        "t": {"terms": {"field": "k", "size": 4, "show_term_doc_count_error": True}, "aggs": {"s": {"sum": {"field": "n"}}}},
        "h": {"histogram": {"field": "n", "interval": 25}, "aggs": {"c": {"value_count": {"field": "d"}}}},
        "dh": {"date_histogram": {"field": "t", "calendar_interval": "quarter"}, "aggs": {"r": {"rate": {"unit": "month", "field": "n"}}}},
        "f": {"filter": {"term": {"b": True}}, "aggs": {"x": {"sum": {"field": "i"}}}},
        "fs": {"filters": {"filters": {"a": {"term": {"b": True}}, "b": {"range": {"n": {"gte": 50}}}}}},
        "r": {"range": {"field": "n", "ranges": [{"to": 30}, {"from": 30}]}},
        "c": {"composite": {"size": 3, "sources": [{"k": {"terms": {"field": "k"}}}]}},
        "gc": {"geo_centroid": {"field": "g"}}, "gt": {"geotile_grid": {"field": "g", "precision": 1}},
        "rt": {"rare_terms": {"field": "tag", "max_doc_count": 1}},
        "ms": {"matrix_stats": {"fields": ["n", "d"]}},
        "am": {"adjacency_matrix": {"filters": {"x": {"term": {"b": True}}, "y": {"term": {"b": False}}}}},
        "mb": {"max_bucket": {"buckets_path": "h>c"}}}}}, typed_keys=True)
    add("random_sampler_query", {"rs": {"random_sampler": {"probability": 0.5, "seed": -3, "shard_seed": 1000}, "aggs": {
        "t": {"terms": {"field": "k"}}, "s": {"sum": {"field": "d"}}, "sig": {"significant_terms": {"field": "k", "min_doc_count": 1}}}}},
        query={"range": {"n": {"gte": 20}}})
    add("random_sampler_all", {"rs": {"random_sampler": {"probability": 1.0, "seed": 5}, "aggs": {"vc": {"value_count": {"field": "n"}}, "t": {"terms": {"field": "b"}}}}})
    add("random_sampler_err_probability", {"rs": {"random_sampler": {"probability": 0.7}, "aggs": {"a": {"avg": {"field": "n"}}}}})
    add("random_sampler_err_negative", {"rs": {"random_sampler": {"probability": -1}, "aggs": {"a": {"avg": {"field": "n"}}}}})
    add("random_sampler_err_unset", {"rs": {"random_sampler": {"seed": 1}, "aggs": {"a": {"avg": {"field": "n"}}}}})
    add("random_sampler_err_no_subs", {"rs": {"random_sampler": {"probability": 0.1}}})
    add("random_sampler_err_parent", {"t": {"terms": {"field": "k"}, "aggs": {"rs": {"random_sampler": {"probability": 0.1}, "aggs": {"a": {"avg": {"field": "n"}}}}}}})
    add("random_sampler_err_sig_min_doc_count", {"rs": {"random_sampler": {"probability": 0.1, "shard_seed": 1}, "aggs": {"s": {"significant_terms": {"field": "k", "min_doc_count": 5}}}}})
    add("random_sampler_err_unsupported", {"rs": {"random_sampler": {"probability": 0.1}, "aggs": {"t": {"terms": {"field": "k"}, "aggs": {"bp": {"boxplot": {"field": "n"}}}}}}})
    return c


JOIN_MAPPING = {
    "properties": {
        "j": {"type": "join", "relations": {"question": ["answer", "note"], "answer": "comment"}},
        "tag": {"type": "keyword"},
        "owner": {"type": "keyword"},
        "votes": {"type": "long"},
        "score": {"type": "double"},
    }
}


def join_docs(seed):
    """Questions, answers (some orphaned), notes and comments, shuffled; `_id` is the position,
    so parent references and routing are resolved after the shuffle."""
    rnd = random.Random(seed)
    docs = []  # (key, parent key or orphan id, root key or orphan id, source)
    for q in range(60):
        docs.append(("q%d" % q, None, "q%d" % q, {"j": "question", "tag": "t%d" % rnd.randrange(6), "score": round(rnd.uniform(0, 10), 2)}))
    answers = []
    for a in range(160):
        q = rnd.randrange(66)  # 60..65: orphans
        parent = "q%d" % q if q < 60 else "x%d" % q
        answers.append(("a%d" % a, parent))
        docs.append(("a%d" % a, parent, parent, {"j": {"name": "answer"}, "owner": "u%d" % rnd.randrange(8), "votes": rnd.randrange(-5, 40)}))
    for n in range(40):
        q = "q%d" % rnd.randrange(60)
        docs.append(("n%d" % n, q, q, {"j": {"name": "note"}, "owner": "u%d" % rnd.randrange(8)}))
    for c in range(220):
        a, root = answers[rnd.randrange(len(answers))]
        docs.append(("c%d" % c, a, root, {"j": {"name": "comment"}, "owner": "u%d" % rnd.randrange(8), "votes": rnd.randrange(0, 5)}))
    rnd.shuffle(docs)
    pos = {d[0]: str(i) for i, d in enumerate(docs)}
    out = []
    for key, parent, root, src in docs:
        src = dict(src)
        if parent is not None:
            src["j"] = {"name": src["j"]["name"], "parent": pos.get(parent, parent)}
        src["_routing"] = pos.get(root, root)
        out.append(src)
    return out


def join_cases():
    c = []

    def add(name, aggs, query=None, typed_keys=False):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": typed_keys})

    add("children", {"c": {"children": {"type": "answer"}, "aggs": {"o": {"terms": {"field": "owner"}}, "v": {"avg": {"field": "votes"}}}},
                     "n": {"children": {"type": "note"}}, "cc": {"children": {"type": "comment"}, "aggs": {"s": {"sum": {"field": "votes"}}}},
                     "u": {"children": {"type": "nope"}, "aggs": {"m": {"max": {"field": "votes"}}}},
                     "q": {"children": {"type": "question"}}}, typed_keys=True)
    add("children_under_terms", {"t": {"terms": {"field": "tag", "size": 4}, "aggs": {"c": {"children": {"type": "answer"}, "aggs": {
        "o": {"terms": {"field": "owner", "size": 3}}, "cm": {"children": {"type": "comment"}, "aggs": {"x": {"max": {"field": "votes"}}}}}}}}})
    add("children_with_query", {"c": {"children": {"type": "answer"}, "aggs": {"v": {"stats": {"field": "votes"}}}}}, query={"term": {"tag": "t1"}})
    add("parent", {"o": {"terms": {"field": "owner", "size": 5}, "aggs": {"p": {"parent": {"type": "answer"}, "aggs": {
        "t": {"terms": {"field": "tag"}}, "s": {"avg": {"field": "score"}}}}}},
        "pc": {"parent": {"type": "comment"}, "aggs": {"v": {"sum": {"field": "votes"}}}},
        "pu": {"parent": {"type": "nope"}}})
    add("parent_with_query", {"p": {"parent": {"type": "answer"}, "aggs": {"t": {"terms": {"field": "tag"}}}}}, query={"range": {"votes": {"gte": 20}}})
    add("join_order_by_children", {"t": {"terms": {"field": "tag", "order": {"c": "desc"}}, "aggs": {"c": {"children": {"type": "answer"}}}}})
    add("join_terms_on_join_field", {"j": {"terms": {"field": "j"}}, "jq": {"terms": {"field": "j#question", "size": 3}}})
    add("children_err_missing_type", {"c": {"children": {}}})
    add("children_err_unknown_key", {"c": {"children": {"tpe": "answer"}}})
    add("parent_err_token", {"p": {"parent": {"type": 3}}})
    add("parent_err_missing_type", {"p": {"parent": {}}})
    return c


BULK_MAPPING = {"properties": {"t": {"type": "date"}, "k": {"type": "keyword"}, "v": {"type": "double"}}}


def bulk_docs(n, seed):
    """Many docs for the collection-time promotions of `auto_date_histogram` and the merge phase
    of `variable_width_histogram`: bursts of nearby timestamps over a year, clustered values."""
    rnd = random.Random(seed)
    docs = []
    start = 1577836800000
    bursts = [start + rnd.randrange(365 * 86400000) for _ in range(40)]
    centers = [rnd.uniform(-1000, 1000) for _ in range(12)]
    for _ in range(n):
        d = {"k": "k%d" % min(int(rnd.expovariate(0.6)), 6)}
        b = bursts[rnd.randrange(len(bursts))]
        d["t"] = b + int(rnd.expovariate(1 / 3600000.0))
        if rnd.random() < 0.95:
            d["v"] = round(rnd.gauss(centers[rnd.randrange(len(centers))], 15), 3)
        docs.append(d)
    return docs


def bulk_cases():
    c = []

    def add(name, aggs, query=None):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": False})

    add("auto_date_histogram_many", {"k": {"terms": {"field": "k", "size": 7}, "aggs": {
        "a": {"auto_date_histogram": {"field": "t", "buckets": 10}, "aggs": {"c": {"value_count": {"field": "v"}}}},
        "b": {"auto_date_histogram": {"field": "t", "buckets": 80, "time_zone": "Asia/Kolkata"}}}}})
    add("auto_date_histogram_single", {"a": {"auto_date_histogram": {"field": "t", "buckets": 25}, "aggs": {"m": {"max": {"field": "v"}}}},
                                       "b": {"auto_date_histogram": {"field": "t", "buckets": 150}},
                                       "c": {"auto_date_histogram": {"field": "t", "buckets": 4, "minimum_interval": "hour"}}})
    add("auto_date_histogram_burst", {"a": {"auto_date_histogram": {"field": "t", "buckets": 30}}, "k": {"terms": {"field": "k"}, "aggs": {
        "a": {"auto_date_histogram": {"field": "t", "buckets": 6}}}}}, query={"range": {"t": {"gte": 1590969600000, "lt": 1593561600000}}})
    add("variable_width_histogram_many", {
        "v": {"variable_width_histogram": {"field": "v", "buckets": 12, "shard_size": 60, "initial_buffer": 300}, "aggs": {"k": {"terms": {"field": "k", "size": 2}}, "a": {"avg": {"field": "v"}}}},
        "w": {"variable_width_histogram": {"field": "v", "buckets": 30}}})
    # CardinalityUpperBound: one range / one filter keep the single-owner mode, samplers don't.
    adh = {"auto_date_histogram": {"field": "t", "buckets": 12}}
    add("auto_date_histogram_cardinality", {
        "r1": {"range": {"field": "v", "ranges": [{"from": -2000}]}, "aggs": {"a": adh}},
        "r2": {"range": {"field": "v", "ranges": [{"to": 0}, {"from": 0}]}, "aggs": {"a": adh}},
        "f1": {"filters": {"filters": {"x": {"exists": {"field": "v"}}}}, "aggs": {"a": adh}},
        "f1o": {"filters": {"filters": {"x": {"exists": {"field": "v"}}}, "other_bucket": True}, "aggs": {"a": adh}},
        "s": {"sampler": {"shard_size": 100000}, "aggs": {"a": adh}},
        "m": {"missing": {"field": "nope"}, "aggs": {"a": adh}},
        "g": {"global": {}, "aggs": {"a": adh}},
        "f": {"filter": {"exists": {"field": "v"}}, "aggs": {"r": {"date_range": {"field": "t", "ranges": [{"from": 0}]}, "aggs": {"a": adh}}}}})
    vw = {"variable_width_histogram": {"field": "v", "buckets": 5}}
    add("variable_width_histogram_one_range", {"r": {"range": {"field": "v", "ranges": [{"from": -2000}]}, "aggs": {"w": vw}},
                                               "f": {"filters": {"filters": [{"exists": {"field": "v"}}]}, "aggs": {"w": vw}}})
    add("variable_width_histogram_err_two_filters", {"f": {"filters": {"filters": [{"exists": {"field": "v"}}, {"exists": {"field": "t"}}]}, "aggs": {"w": vw}}})
    add("variable_width_histogram_err_other_bucket", {"f": {"filters": {"filters": [{"exists": {"field": "v"}}], "other_bucket": True}, "aggs": {"w": vw}}})
    add("variable_width_histogram_err_sampler", {"s": {"sampler": {}, "aggs": {"w": vw}}})
    return c


TS_MAPPING = {"properties": {
    "@timestamp": {"type": "date"}, "host": {"type": "keyword", "time_series_dimension": True},
    "pod": {"type": "keyword", "time_series_dimension": True}, "zone": {"type": "long", "time_series_dimension": True},
    "up": {"type": "boolean", "time_series_dimension": True}, "cpu": {"type": "double", "time_series_metric": "gauge"},
    "mem": {"type": "long", "time_series_metric": "gauge"}, "seq": {"type": "long"},
    "bytes": {"type": "long", "time_series_metric": "counter"},
}}
TS_SETTINGS = {"index.mode": "time_series", "index.routing_path": ["host", "pod"],
               "index.time_series.start_time": "2020-01-01T00:00:00Z", "index.time_series.end_time": "2030-01-01T00:00:00Z"}


def ts_docs(n, seed):
    """Samples of ~40 series (some with fewer dimensions) every few minutes."""
    rnd = random.Random(seed)
    series = []
    for h in range(8):
        for p in range(rnd.randrange(1, 7)):
            s = {"host": "h%d" % h, "pod": "p%d" % p}
            r = rnd.random()
            if r < 0.6:
                s["zone"] = rnd.randrange(-3, 4)
            if r < 0.3:
                s["up"] = rnd.random() < 0.5
            series.append(s)
    t0 = 1609459200000
    docs, seen = [], set()
    while len(docs) < n:
        s = series[min(int(rnd.expovariate(0.08)), len(series) - 1)]
        t = t0 + rnd.randrange(6 * 3600) * 1000
        k = (json.dumps(s, sort_keys=True), t)
        if k in seen:
            continue
        seen.add(k)
        d = dict(s)
        d["@timestamp"] = t
        d["cpu"] = round(rnd.uniform(0, 100), 2)
        d["mem"] = rnd.randrange(1 << 30)
        docs.append(d)
    # A counter per series: increasing in time, with occasional resets.
    crnd = random.Random(seed + 7)
    by_series = {}
    for d in docs:
        by_series.setdefault(tuple(sorted((k, str(v)) for k, v in d.items() if k in ("host", "pod", "zone", "up"))), []).append(d)
    for samples in by_series.values():
        value = crnd.randrange(1000)
        for d in sorted(samples, key=lambda d: d["@timestamp"]):
            value = crnd.randrange(50) if crnd.random() < 0.05 else value + crnd.randrange(500)
            d["bytes"] = value
    return docs


def ts_cases():
    c = []

    def add(name, aggs, query=None):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": False})

    add("time_series", {"ts": {"time_series": {}, "aggs": {"a": {"avg": {"field": "cpu"}}, "m": {"max": {"field": "mem"}}}},
                        "tk": {"time_series": {"keyed": False}}})
    add("time_series_size", {"s1": {"time_series": {"size": 1}}, "s5": {"time_series": {"size": 5, "keyed": False}}, "s0": {"time_series": {"size": 0}}})
    add("time_series_under_terms", {"t": {"terms": {"field": "host", "size": 3}, "aggs": {"ts": {"time_series": {"keyed": False, "size": 2}, "aggs": {"s": {"sum": {"field": "cpu"}}}}}}})
    add("time_series_under_histogram", {"d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "1h"}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_sub_histogram", {"ts": {"time_series": {"keyed": False}, "aggs": {"d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "2h"}, "aggs": {"a": {"avg": {"field": "cpu"}}}}}}})
    add("time_series_query", {"ts": {"time_series": {}}}, query={"range": {"cpu": {"gte": 50}}})
    add("time_series_pipeline", {"ts": {"time_series": {"keyed": False}, "aggs": {"a": {"avg": {"field": "cpu"}}}}, "mx": {"max_bucket": {"buckets_path": "ts>a"}}})
    add("time_series_err_field", {"ts": {"time_series": {"foo": 1}}})
    add("counter_rate", {"ts": {"time_series": {"keyed": False}, "aggs": {
        "r": {"rate": {"field": "bytes"}}, "m": {"rate": {"field": "bytes", "unit": "minute"}}, "h": {"rate": {"field": "bytes", "unit": "hour"}},
        "mo": {"rate": {"field": "bytes", "unit": "month"}}, "q": {"rate": {"field": "bytes", "unit": "quarter"}}, "y": {"rate": {"field": "bytes", "unit": "year"}},
        "v": {"rate": {"field": "bytes", "mode": "value_count"}}, "a": {"avg": {"field": "bytes"}}}}})
    add("counter_rate_per_hour", {"ts": {"time_series": {"keyed": False, "size": 6}, "aggs": {"d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "1h"}, "aggs": {
        "r": {"rate": {"field": "bytes"}}}}}}})
    add("counter_rate_under_histogram", {"d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "2h"}, "aggs": {"ts": {"time_series": {}, "aggs": {"r": {"rate": {"field": "bytes"}}}}}}})
    add("counter_rate_beside_time_series", {"ts": {"time_series": {"size": 1}}, "r": {"rate": {"field": "bytes"}}, "d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "3h"}, "aggs": {"r": {"rate": {"field": "bytes"}}}}})
    add("counter_rate_query", {"ts": {"time_series": {}, "aggs": {"r": {"rate": {"field": "bytes", "unit": "second"}}}}}, query={"range": {"cpu": {"gte": 30}}})
    add("counter_metrics", {"mx": {"max": {"field": "bytes"}}, "s": {"stats": {"field": "bytes"}}, "c": {"cardinality": {"field": "bytes"}}, "h": {"histogram": {"field": "bytes", "interval": 100000}}})
    add("counter_err_rate_outside", {"r": {"rate": {"field": "bytes"}}})
    add("counter_err_rate_in_date_histogram", {"d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "1h"}, "aggs": {"r": {"rate": {"field": "bytes"}}}}})
    add("counter_err_deferable", {"t": {"terms": {"field": "host"}, "aggs": {"ts": {"time_series": {}, "aggs": {"r": {"rate": {"field": "bytes"}}}}}}})
    add("counter_err_empty_bucket", {"ts": {"time_series": {}, "aggs": {"f": {"filter": {"term": {"host": "zzz"}}, "aggs": {"r": {"rate": {"field": "bytes"}}}}}}})
    add("counter_err_terms", {"t": {"terms": {"field": "bytes"}}})
    add("counter_err_date_histogram", {"t": {"date_histogram": {"field": "bytes", "fixed_interval": "1d"}}})
    add("counter_err_missing", {"t": {"missing": {"field": "bytes"}}})
    add("counter_err_composite", {"t": {"composite": {"sources": [{"b": {"terms": {"field": "bytes"}}}]}}})
    return c


PRE_MAPPING = {"properties": {
    "h": {"type": "histogram"},
    "m": {"type": "aggregate_metric_double", "metrics": ["min", "max", "sum", "value_count"], "default_metric": "max"},
    "m2": {"type": "aggregate_metric_double", "metrics": ["max", "value_count"], "default_metric": "max"},
    "k": {"type": "keyword"}, "t": {"type": "date"}, "n": {"type": "long"},
}}


def pre_docs(n, seed):
    """Pre-aggregated values: latency-like histograms (with zero counts) and downsampled metrics."""
    rnd = random.Random(seed)
    docs = []
    for i in range(n):
        d = {"k": "k%d" % min(int(rnd.expovariate(0.7)), 4), "t": 1609459200000 + rnd.randrange(5 * 86400) * 1000, "n": rnd.randrange(100)}
        if rnd.random() < 0.85:
            vals, v = [], round(rnd.uniform(0, 2), 3)
            for _ in range(rnd.randrange(1, 9)):
                vals.append(v)
                v = round(v + rnd.choice([0.001, 0.05, 0.5, 3.7]), 3)
            d["h"] = {"values": vals, "counts": [rnd.choice([0, 1, 2, 3, 7, 40]) for _ in vals]}
        if rnd.random() < 0.9:
            lo = round(rnd.uniform(-50, 50), 2)
            hi = round(lo + rnd.uniform(0, 30), 2)
            c = rnd.randrange(1, 60)
            d["m"] = {"min": lo, "max": hi, "sum": round(rnd.uniform(lo, hi) * c, 2), "value_count": c}
            d["m2"] = {"max": hi, "value_count": c}
        docs.append(d)
    return docs


def pre_cases():
    c = []

    def add(name, aggs, query=None, typed_keys=False):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": typed_keys})

    add("histogram_field_metrics", {"a": {"avg": {"field": "h"}}, "s": {"sum": {"field": "h"}}, "mi": {"min": {"field": "h"}}, "ma": {"max": {"field": "h"}},
                                    "v": {"value_count": {"field": "h"}}}, typed_keys=True)
    add("histogram_field_percentiles", {"p": {"percentiles": {"field": "h"}}, "pc": {"percentiles": {"field": "h", "tdigest": {"compression": 30}, "percents": [10, 50, 90, 99.9]}},
                                        "ph": {"percentiles": {"field": "h", "hdr": {"number_of_significant_value_digits": 3}}},
                                        "r": {"percentile_ranks": {"field": "h", "values": [0.1, 0.5, 2, 4]}},
                                        "rh": {"percentile_ranks": {"field": "h", "values": [0.1, 0.5, 2], "hdr": {}}},
                                        "b": {"boxplot": {"field": "h"}}})
    add("histogram_field_err_histogram_subs", {"h": {"histogram": {"field": "h", "interval": 0.5}, "aggs": {"s": {"sum": {"field": "n"}}}}})
    add("histogram_field_err_range_subs", {"r": {"range": {"field": "h", "ranges": [{"to": 0.5}]}, "aggs": {"v": {"value_count": {"field": "h"}}}}})
    add("histogram_field_histogram", {"h": {"histogram": {"field": "h", "interval": 0.5}},
                                      "o": {"histogram": {"field": "h", "interval": 2, "offset": 0.25, "min_doc_count": 0, "extended_bounds": {"min": -4, "max": 20}}},
                                      "hb": {"histogram": {"field": "h", "interval": 1, "hard_bounds": {"min": 0.5, "max": 3}}}})
    add("histogram_field_range", {"r": {"range": {"field": "h", "ranges": [{"to": 0.5}, {"from": 0.25, "to": 2}, {"from": 1}], "keyed": True}},
                                  "nr": {"range": {"field": "h", "ranges": [{"to": 1}, {"from": 1, "to": 3}, {"from": 3}]}}})
    add("histogram_field_rate", {"d": {"date_histogram": {"field": "t", "calendar_interval": "day"}, "aggs": {"r": {"rate": {"field": "h"}}, "v": {"rate": {"field": "h", "mode": "value_count", "unit": "hour"}}}}})
    add("histogram_field_under_terms", {"k": {"terms": {"field": "k"}, "aggs": {"a": {"avg": {"field": "h"}}, "p": {"percentiles": {"field": "h", "percents": [50]}},
                                                                                "h": {"histogram": {"field": "h", "interval": 1}}}}})
    add("aggregate_metric", {"a": {"avg": {"field": "m"}}, "s": {"sum": {"field": "m"}}, "mi": {"min": {"field": "m"}}, "ma": {"max": {"field": "m"}},
                             "v": {"value_count": {"field": "m"}}, "a2": {"avg": {"field": "m2"}}, "s2": {"sum": {"field": "m2"}}, "mi2": {"min": {"field": "m2"}},
                             "ma2": {"max": {"field": "m2"}}, "v2": {"value_count": {"field": "m2"}}}, typed_keys=True)
    add("aggregate_metric_under_terms", {"k": {"terms": {"field": "k", "order": {"a": "desc"}}, "aggs": {"a": {"avg": {"field": "m"}}, "mx": {"max": {"field": "m"}}}},
                                         "d": {"date_histogram": {"field": "t", "calendar_interval": "day"}, "aggs": {"s": {"sum": {"field": "m"}}}}})
    for name, aggs in [
        ("histogram_field_err_stats", {"x": {"stats": {"field": "h"}}}),
        ("histogram_field_err_extended_stats", {"x": {"extended_stats": {"field": "h"}}}),
        ("histogram_field_err_cardinality", {"x": {"cardinality": {"field": "h"}}}),
        ("histogram_field_err_mad", {"x": {"median_absolute_deviation": {"field": "h"}}}),
        ("histogram_field_err_terms", {"x": {"terms": {"field": "h"}}}),
        ("histogram_field_err_missing", {"x": {"missing": {"field": "h"}}}),
        ("histogram_field_err_date_range", {"x": {"date_range": {"field": "h", "ranges": [{"to": 1}]}}}),
        ("histogram_field_err_composite_terms", {"x": {"composite": {"sources": [{"a": {"terms": {"field": "h"}}}]}}}),
        ("histogram_field_err_composite_histogram", {"x": {"composite": {"sources": [{"a": {"histogram": {"field": "h", "interval": 1}}}]}}}),
        ("histogram_field_err_weighted_avg", {"x": {"weighted_avg": {"value": {"field": "h"}, "weight": {"field": "n"}}}}),
        ("histogram_field_err_missing_param", {"x": {"avg": {"field": "h", "missing": 3}}}),
        ("histogram_field_err_format", {"x": {"avg": {"field": "h", "format": "0.0"}}}),
        ("aggregate_metric_err_stats", {"x": {"stats": {"field": "m"}}}),
        ("aggregate_metric_err_percentiles", {"x": {"percentiles": {"field": "m"}}}),
        ("aggregate_metric_err_histogram", {"x": {"histogram": {"field": "m", "interval": 2}}}),
        ("aggregate_metric_err_cardinality", {"x": {"cardinality": {"field": "m"}}}),
        ("aggregate_metric_err_terms", {"x": {"terms": {"field": "m"}}}),
        ("aggregate_metric_err_missing", {"x": {"missing": {"field": "m"}}}),
        ("aggregate_metric_err_rate", {"d": {"date_histogram": {"field": "t", "calendar_interval": "day"}, "aggs": {"x": {"rate": {"field": "m"}}}}}),
        ("aggregate_metric_err_geo_bounds", {"x": {"geo_bounds": {"field": "m"}}}),
        ("aggregate_metric_err_missing_param", {"x": {"avg": {"field": "m", "missing": 3}}}),
        ("aggregate_metric_err_t_test", {"x": {"t_test": {"a": {"field": "m"}, "b": {"field": "n"}}}}),
        ("weighted_avg_err_keyword", {"x": {"weighted_avg": {"value": {"field": "k"}, "weight": {"field": "n"}}}}),
    ]:
        add(name, aggs)
    return c


EXP_MAPPING = {"properties": {
    "e": {"type": "exponential_histogram"}, "e2": {"type": "exponential_histogram"},
    "k": {"type": "keyword"}, "t": {"type": "date"}, "n": {"type": "long"},
}}


def exp_hist(rnd, scale, nbuckets, center, spread):
    """Buckets around 2^center at `scale`: indices of sampled log2 values, random counts."""
    side = {}
    for _ in range(nbuckets):
        log2 = rnd.gauss(center, spread)
        side[int(log2 * 2 ** scale) if scale >= 0 else int(log2) >> -scale] = rnd.choice([1, 1, 2, 3, 5, 8, 40, 300])
    idx = sorted(side)
    return {"indices": idx, "counts": [side[i] for i in idx]}


def exp_docs(n, seed):
    """Exponential histograms: positive and negative buckets at many scales, zero buckets that
    overlap buckets, given or estimated sum/min/max, empty histograms; `e2` has hundreds of
    buckets at scale 20 so merges must downscale."""
    rnd = random.Random(seed)
    docs = []
    for i in range(n):
        d = {"k": "k%d" % min(int(rnd.expovariate(0.6)), 5), "t": 1609459200000 + rnd.randrange(4 * 86400) * 1000, "n": rnd.randrange(100)}
        if rnd.random() < 0.85:
            scale = rnd.choice([-3, -1, 0, 1, 2, 3, 5, 8, 12, 20])
            h = {"scale": scale}
            if rnd.random() < 0.05:
                pass  # empty
            else:
                center, spread = rnd.uniform(-8, 12), rnd.choice([0.05, 0.3, 1.0, 3.0])
                h["positive"] = exp_hist(rnd, scale, rnd.randrange(1, 16), center, spread)
                if rnd.random() < 0.3:
                    h["negative"] = exp_hist(rnd, scale, rnd.randrange(1, 8), rnd.uniform(-4, 8), spread)
                if rnd.random() < 0.3:
                    h["zero"] = {"threshold": rnd.choice([0.0, 1e-9, 0.001, 0.5, 2.0]), "count": rnd.randrange(0, 12)}
                if rnd.random() < 0.5:
                    pos = h["positive"]["indices"]
                    base = 2 ** (2 ** -scale)
                    lo = base ** pos[0] if "negative" not in h else -(base ** (h["negative"]["indices"][-1] + 1))
                    if "zero" in h and h["zero"]["count"] > 0 and "negative" not in h:
                        lo = -h["zero"]["threshold"]
                    hi = base ** (pos[-1] + 1)
                    h["min"] = round(lo * 1.0001 if lo > 0 else lo * 0.9999, 6)
                    h["max"] = round(hi * 0.9999, 6)
                    h["sum"] = round(rnd.uniform(-10, 1000), 3)
            d["e"] = h
        if rnd.random() < 0.25:
            start = rnd.randrange(-2000000, 4000000)
            nb = rnd.randrange(150, 420)
            idx = sorted(set(start + rnd.randrange(0, nb * 3) for _ in range(nb)))
            d["e2"] = {"scale": 20, "positive": {"indices": idx, "counts": [rnd.randrange(1, 9) for _ in idx]}}
        docs.append(d)
    return docs


def exp_cases():
    c = []

    def add(name, aggs, query=None, typed_keys=False):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": typed_keys})

    add("exp_metrics", {"a": {"avg": {"field": "e"}}, "s": {"sum": {"field": "e"}}, "mi": {"min": {"field": "e"}}, "ma": {"max": {"field": "e"}},
                        "v": {"value_count": {"field": "e"}}, "a2": {"avg": {"field": "e2"}}, "v2": {"value_count": {"field": "e2"}}}, typed_keys=True)
    add("exp_percentiles", {"p": {"percentiles": {"field": "e"}},
                            "pc": {"percentiles": {"field": "e", "tdigest": {"compression": 30}, "percents": [0, 10, 50, 90, 99.9, 100]}},
                            "pk": {"percentiles": {"field": "e", "keyed": False, "percents": [25, 75]}},
                            "ph": {"percentiles": {"field": "e", "tdigest": {"execution_hint": "high_accuracy"}, "percents": [33]}},
                            "r": {"percentile_ranks": {"field": "e", "values": [-5, -0.5, 0, 0.001, 1, 2, 100, 100000]}},
                            "b": {"boxplot": {"field": "e"}},
                            "p2": {"percentiles": {"field": "e2"}}, "b2": {"boxplot": {"field": "e2"}},
                            "r2": {"percentile_ranks": {"field": "e2", "values": [1, 4, 16, 64]}}}, typed_keys=True)
    add("exp_under_terms", {"k": {"terms": {"field": "k", "order": {"p.50": "desc"}},
                                  "aggs": {"p": {"percentiles": {"field": "e", "percents": [50, 95]}}, "b": {"boxplot": {"field": "e"}},
                                           "a": {"avg": {"field": "e"}}, "mx": {"max": {"field": "e"}}}},
                            "kb": {"terms": {"field": "k", "order": {"b.q3": "asc"}}, "aggs": {"b": {"boxplot": {"field": "e2"}}}},
                            "km": {"terms": {"field": "k", "order": {"mi": "asc"}}, "aggs": {"mi": {"min": {"field": "e"}}}}})
    add("exp_histogram_too_many", {"h": {"histogram": {"field": "e", "interval": 0.5}}})
    add("exp_histogram", {"h": {"histogram": {"field": "e", "interval": 64}},
                          "o": {"histogram": {"field": "e", "interval": 300, "offset": 0.25, "min_doc_count": 0, "extended_bounds": {"min": -900, "max": 3000}}},
                          "hb": {"histogram": {"field": "e", "interval": 1, "hard_bounds": {"min": -3, "max": 5}}},
                          "h2": {"histogram": {"field": "e2", "interval": 2, "min_doc_count": 3}}})
    add("exp_range", {"r": {"range": {"field": "e", "ranges": [{"to": 0}, {"from": -1, "to": 1}, {"from": 0.5, "to": 10}, {"from": 1}], "keyed": True}},
                      "nr": {"range": {"field": "e", "ranges": [{"to": -10}, {"from": -10, "to": 0}, {"from": 0, "to": 0.001}, {"from": 0.001, "to": 4}, {"from": 4}]}},
                      "r2": {"range": {"field": "e2", "ranges": [{"to": 1}, {"from": 1, "to": 2}, {"from": 2}]}}})
    add("exp_date_histogram", {"d": {"date_histogram": {"field": "t", "calendar_interval": "day"},
                                     "aggs": {"s": {"sum": {"field": "e"}}, "p": {"percentiles": {"field": "e", "percents": [50, 99]}}, "v": {"value_count": {"field": "e"}}}}})
    add("exp_filtered", {"p": {"percentiles": {"field": "e"}}, "b": {"boxplot": {"field": "e"}}}, query={"range": {"n": {"lt": 30}}})
    add("exp_empty", {"p": {"percentiles": {"field": "e"}}, "r": {"percentile_ranks": {"field": "e", "values": [1]}}, "b": {"boxplot": {"field": "e"}},
                      "s": {"sum": {"field": "e"}}, "mi": {"min": {"field": "e"}}, "a": {"avg": {"field": "e"}}}, query={"term": {"k": "nope"}})
    add("exp_bucket_metrics", {"k": {"terms": {"field": "k"}, "aggs": {"p": {"percentiles": {"field": "e", "percents": [50]}}}},
                               "mx": {"max_bucket": {"buckets_path": "k>p[50.0]"}}})
    for name, aggs in [
        ("exp_err_hdr", {"x": {"percentiles": {"field": "e", "hdr": {}}}}),
        ("exp_err_hdr_ranks", {"x": {"percentile_ranks": {"field": "e", "values": [1], "hdr": {}}}}),
        ("exp_err_stats", {"x": {"stats": {"field": "e"}}}),
        ("exp_err_extended_stats", {"x": {"extended_stats": {"field": "e"}}}),
        ("exp_err_cardinality", {"x": {"cardinality": {"field": "e"}}}),
        ("exp_err_missing", {"x": {"missing": {"field": "e"}}}),
        ("exp_err_rate", {"d": {"date_histogram": {"field": "t", "calendar_interval": "day"}, "aggs": {"x": {"rate": {"field": "e"}}}}}),
        ("exp_err_mad", {"x": {"median_absolute_deviation": {"field": "e"}}}),
        ("exp_err_terms", {"x": {"terms": {"field": "e"}}}),
        ("exp_err_rare_terms", {"x": {"rare_terms": {"field": "e"}}}),
        ("exp_err_significant_terms", {"x": {"significant_terms": {"field": "e"}}}),
        ("exp_err_multi_terms", {"x": {"multi_terms": {"terms": [{"field": "e"}, {"field": "k"}]}}}),
        ("exp_err_top_metrics", {"x": {"top_metrics": {"metrics": {"field": "e"}, "sort": {"n": "asc"}}}}),
        ("exp_err_weighted_avg", {"x": {"weighted_avg": {"value": {"field": "e"}, "weight": {"field": "n"}}}}),
        ("exp_err_histogram_subs", {"x": {"histogram": {"field": "e", "interval": 1}, "aggs": {"s": {"sum": {"field": "n"}}}}}),
        ("exp_err_range_subs", {"x": {"range": {"field": "e", "ranges": [{"to": 1}]}, "aggs": {"s": {"sum": {"field": "n"}}}}}),
        ("exp_err_missing_param", {"x": {"sum": {"field": "e", "missing": 3}}}),
        ("exp_err_format", {"x": {"sum": {"field": "e", "format": "0.0"}}}),
        ("exp_err_composite_terms", {"x": {"composite": {"sources": [{"a": {"terms": {"field": "e"}}}]}}}),
        ("exp_err_composite_histogram", {"x": {"composite": {"sources": [{"a": {"histogram": {"field": "e", "interval": 1}}}]}}}),
        ("exp_err_date_histogram", {"x": {"date_histogram": {"field": "e", "fixed_interval": "1d"}}}),
        ("exp_err_date_range", {"x": {"date_range": {"field": "e", "ranges": [{"to": 1}]}}}),
        ("exp_err_auto_date_histogram", {"x": {"auto_date_histogram": {"field": "e"}}}),
        ("exp_err_variable_width", {"x": {"variable_width_histogram": {"field": "e"}}}),
        ("exp_err_matrix_stats", {"x": {"matrix_stats": {"fields": ["e", "n"]}}}),
        ("exp_err_string_stats", {"x": {"string_stats": {"field": "e"}}}),
        ("exp_err_geo_bounds", {"x": {"geo_bounds": {"field": "e"}}}),
        ("exp_err_geohash_grid", {"x": {"geohash_grid": {"field": "e"}}}),
        ("exp_err_ip_range", {"x": {"ip_range": {"field": "e", "ranges": [{"to": "10.0.0.1"}]}}}),
        ("exp_err_t_test", {"x": {"t_test": {"a": {"field": "e"}, "b": {"field": "n"}}}}),
        ("exp_err_diversified_sampler", {"x": {"diversified_sampler": {"field": "e"}, "aggs": {"s": {"sum": {"field": "n"}}}}}),
        ("exp_err_cumulative_cardinality_src", {"x": {"cardinality": {"field": "e"}}}),
    ]:
        add(name, aggs)
    return c


SHAPE_MAPPING = {"properties": {
    "s": {"type": "geo_shape"}, "s2": {"type": "geo_shape"}, "xy": {"type": "shape"},
    "k": {"type": "keyword"}, "n": {"type": "long"},
}}


def star(rnd, cx, cy, rad, n, ccw, lo_x=-179.9, hi_x=179.9, lo_y=-89.9, hi_y=89.9):
    import math
    # Evenly spread jittered angles: the ring stays simple and contains a disc around the center.
    angles = [(i + rnd.uniform(0, 0.5)) / n * 2 * math.pi for i in range(n)]
    if not ccw:
        angles = [-a for a in angles]
    ring = []
    for a in angles:
        d = rad * rnd.uniform(0.6, 1.0)
        ring.append([round(min(hi_x, max(lo_x, cx + d * math.cos(a))), 6), round(min(hi_y, max(lo_y, cy + d * math.sin(a))), 6)])
    ring.append(ring[0])
    return ring


def shape_value(rnd, cx, cy, rad, geo=True):
    kind = rnd.choice(["point", "line", "polygon", "polygon", "envelope", "multipoint", "collection"])
    lim = (-179.9, 179.9, -89.9, 89.9) if geo else (-1e4, 1e4, -1e4, 1e4)
    clamp = lambda x, y: [round(min(lim[1], max(lim[0], x)), 6), round(min(lim[3], max(lim[2], y)), 6)]
    if kind == "point":
        return {"type": "Point", "coordinates": clamp(cx, cy)}
    if kind == "line":
        return {"type": "LineString", "coordinates": [clamp(cx + rnd.uniform(-rad, rad), cy + rnd.uniform(-rad, rad)) for _ in range(rnd.randrange(2, 6))]}
    if kind == "multipoint":
        return {"type": "MultiPoint", "coordinates": [clamp(cx + rnd.uniform(-rad, rad), cy + rnd.uniform(-rad, rad)) for _ in range(rnd.randrange(1, 5))]}
    if kind == "envelope":
        return {"type": "envelope", "coordinates": [clamp(cx - rad, cy + rad), clamp(cx + rad, cy - rad)]}
    if kind == "collection":
        return {"type": "GeometryCollection", "geometries": [shape_value(rnd, cx + rnd.uniform(-rad, rad), cy + rnd.uniform(-rad, rad), rad / 3, geo) for _ in range(2)]}
    rings = [star(rnd, cx, cy, rad, rnd.randrange(5, 10), True, *lim)]
    if rnd.random() < 0.3:
        rings.append(star(rnd, cx, cy, rad * 0.2, rnd.randrange(3, 6), False, *lim))
    return {"type": "Polygon", "coordinates": rings}


def shape_docs(n, seed):
    """geo_shape and shape values: points, lines, polygons with holes, envelopes, collections; `s2`
    sits on both sides of the antimeridian so wrapped and unwrapped bounds differ."""
    rnd = random.Random(seed)
    docs = []
    for i in range(n):
        d = {"k": "k%d" % min(int(rnd.expovariate(0.6)), 4), "n": rnd.randrange(100)}
        if rnd.random() < 0.85:
            d["s"] = shape_value(rnd, rnd.uniform(-120, 120), rnd.uniform(-60, 60), rnd.choice([0.01, 0.5, 3, 10]))
            if rnd.random() < 0.1:
                d["s"] = [d["s"], shape_value(rnd, rnd.uniform(-120, 120), rnd.uniform(-60, 60), 1)]
        if rnd.random() < 0.5:
            lon = rnd.choice([rnd.uniform(165, 178), rnd.uniform(-178, -165)])
            d["s2"] = shape_value(rnd, lon, rnd.uniform(-30, 30), rnd.choice([0.1, 1]))
        if rnd.random() < 0.7:
            d["xy"] = shape_value(rnd, rnd.uniform(-5000, 5000), rnd.uniform(-5000, 5000), rnd.choice([1, 50, 400]), geo=False)
        docs.append(d)
    return docs


def shape_cases():
    c = []

    def add(name, aggs, query=None, typed_keys=False):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": typed_keys})

    add("shape_bounds", {"b": {"geo_bounds": {"field": "s"}}, "bw": {"geo_bounds": {"field": "s", "wrap_longitude": False}},
                         "b2": {"geo_bounds": {"field": "s2"}}, "b2w": {"geo_bounds": {"field": "s2", "wrap_longitude": False}},
                         "x": {"cartesian_bounds": {"field": "xy"}}}, typed_keys=True)
    add("shape_bounds_under_terms", {"k": {"terms": {"field": "k", "order": {"_key": "asc"}}, "aggs": {"b": {"geo_bounds": {"field": "s"}}, "x": {"cartesian_bounds": {"field": "xy"}}}}})
    add("shape_bounds_filtered", {"b": {"geo_bounds": {"field": "s2"}}, "x": {"cartesian_bounds": {"field": "xy"}}}, query={"range": {"n": {"lt": 20}}})
    add("shape_bounds_empty", {"b": {"geo_bounds": {"field": "s"}}, "x": {"cartesian_bounds": {"field": "xy"}}}, query={"term": {"k": "nope"}})
    add("shape_bounds_missing", {"b": {"geo_bounds": {"field": "s2", "missing": "POINT (10 20)"}},
                                 "x": {"cartesian_bounds": {"field": "xy", "missing": "BBOX (1, 2, 4, 3)"}}})
    add("shape_bounds_sparse_buckets", {"k": {"terms": {"field": "k"}, "aggs": {"b": {"geo_bounds": {"field": "s2"}}}}}, query={"range": {"n": {"lt": 8}}})
    add("shape_bounds_unmapped_sibling", {"b": {"geo_bounds": {"field": "nope"}}, "f": {"filter": {"exists": {"field": "s2"}}, "aggs": {"b": {"geo_bounds": {"field": "s2"}}}}})
    for name, aggs in [
        ("shape_err_missing_wkt", {"x": {"geo_bounds": {"field": "s", "missing": "1,1"}}}),
        ("shape_err_value_count", {"x": {"value_count": {"field": "s"}}}),
        ("shape_err_cardinality", {"x": {"cardinality": {"field": "s"}}}),
        ("shape_err_terms", {"x": {"terms": {"field": "s"}}}),
        ("shape_err_missing", {"x": {"missing": {"field": "s"}}}),
        ("shape_err_geo_distance", {"x": {"geo_distance": {"field": "s", "origin": "0,0", "ranges": [{"to": 1}]}}}),
        ("shape_err_top_metrics", {"x": {"top_metrics": {"metrics": {"field": "s"}, "sort": {"n": "asc"}}}}),
        ("shape_err_geo_bounds_on_shape", {"x": {"geo_bounds": {"field": "xy"}}}),
        ("shape_err_cartesian_bounds_on_geo", {"x": {"cartesian_bounds": {"field": "s"}}}),
        ("shape_err_avg", {"x": {"avg": {"field": "s"}}}),
        ("shape_err_histogram", {"x": {"histogram": {"field": "s", "interval": 1}}}),
        ("shape_err_xy_value_count", {"x": {"value_count": {"field": "xy"}}}),
        ("shape_err_composite", {"x": {"composite": {"sources": [{"t": {"geotile_grid": {"field": "s"}}}]}}}),
        ("shape_err_format", {"x": {"geo_bounds": {"field": "s", "format": "x"}}}),
        ("shape_err_rare_terms", {"x": {"rare_terms": {"field": "s"}}}),
        ("shape_err_significant_terms", {"x": {"significant_terms": {"field": "s"}}}),
        ("shape_err_multi_terms", {"x": {"multi_terms": {"terms": [{"field": "s"}, {"field": "k"}]}}}),
        ("shape_err_diversified_sampler", {"x": {"diversified_sampler": {"field": "s"}, "aggs": {"m": {"max": {"field": "n"}}}}}),
        ("shape_err_string_stats", {"x": {"string_stats": {"field": "s"}}}),
        ("shape_err_matrix_stats", {"x": {"matrix_stats": {"fields": ["s", "n"]}}}),
        ("shape_err_weighted_avg", {"x": {"weighted_avg": {"value": {"field": "s"}, "weight": {"field": "n"}}}}),
        ("shape_err_mad", {"x": {"median_absolute_deviation": {"field": "s"}}}),
        ("shape_err_percentiles", {"x": {"percentiles": {"field": "s"}}}),
        ("shape_err_boxplot", {"x": {"boxplot": {"field": "xy"}}}),
        ("shape_err_range", {"x": {"range": {"field": "s", "ranges": [{"to": 1}]}}}),
        ("shape_err_date_histogram", {"x": {"date_histogram": {"field": "s", "fixed_interval": "1d"}}}),
        ("shape_err_variable_width", {"x": {"variable_width_histogram": {"field": "xy"}}}),
        ("shape_err_ip_range", {"x": {"ip_range": {"field": "s", "ranges": [{"to": "10.0.0.1"}]}}}),
        ("shape_err_stats", {"x": {"stats": {"field": "xy"}}}),
    ]:
        add(name, aggs)
    return c


RANGE_MAPPING = {"properties": {
    "k": {"type": "keyword"}, "n": {"type": "long"},
    "r": {"type": "long_range"}, "ir": {"type": "integer_range"}, "fr": {"type": "float_range"},
    "dr": {"type": "double_range"}, "tr": {"type": "date_range"}, "ipr": {"type": "ip_range"},
    "open": {"type": "long_range"}, "ipr2": {"type": "ip_range"}, "xy": {"type": "point"},
}}


def range_docs(n, seed):
    """Range fields: 0-3 ranges per doc with inclusive and exclusive bounds (bounded, except
    `open`, which only metrics and `missing` read)."""
    rnd = random.Random(seed)
    docs = []
    t0 = 1577836800000  # 2020-01-01

    def bounds(lo, hi, conv):
        r = {}
        r["gt" if rnd.random() < 0.3 else "gte"] = conv(lo)
        r["lt" if rnd.random() < 0.3 else "lte"] = conv(hi)
        return r

    for _ in range(n):
        d = {"k": "k%d" % min(int(rnd.expovariate(0.5)), 5)}
        if rnd.random() < 0.8:
            d["n"] = rnd.randrange(100)
        for f in ["r", "ir", "fr", "dr", "tr"]:
            m = rnd.choice([0, 1, 1, 1, 2, 3]) if rnd.random() < 0.9 else 0
            items = []
            for _ in range(m):
                if f in ("r", "ir"):
                    a = rnd.randrange(-50, 100)
                    items.append(bounds(a, a + rnd.randrange(2, 30), int))
                elif f == "fr":
                    a = round(rnd.uniform(-20, 40), 2)
                    items.append(bounds(a, a + round(rnd.uniform(0.1, 8), 2), float))
                elif f == "dr":
                    a = round(rnd.uniform(-200, 600), 3)
                    items.append(bounds(a, a + round(rnd.uniform(0.1, 90), 3), float))
                else:
                    a = t0 + rnd.randrange(366 * 86400000)
                    items.append(bounds(a, a + rnd.randrange(2, 60 * 86400000), int))
            if m == 1:
                d[f] = items[0]
            elif m > 1:
                d[f] = items
        if rnd.random() < 0.85:
            ips = []
            for _ in range(rnd.choice([1, 1, 2])):
                if rnd.random() < 0.7:
                    ips.append("10.%d.%d.0/%d" % (rnd.randrange(4), rnd.randrange(8), rnd.choice([24, 28, 30])))
                else:
                    ips.append("2001:db8:%x::/%d" % (rnd.randrange(16), rnd.choice([48, 64])))
            # Elasticsearch encodes several ip ranges in `HashSet` order, which depends on
            # identity hash codes: only single ranges have a reproducible encoding.
            d["ipr"] = ips[0]
            if len(ips) > 1:
                d["ipr2"] = ips
        r = rnd.random()
        if r < 0.3:
            d["open"] = {"gte": rnd.randrange(1000)}
        elif r < 0.5:
            d["open"] = {"lt": rnd.randrange(1000)}
        elif r < 0.8:
            a = rnd.randrange(1000)
            d["open"] = [{"gte": a, "lte": a + 5}, {"gt": a + 50}]
        docs.append(d)
    # Cartesian points, from their own generator so the range fields keep their values.
    prnd = random.Random(seed + 1)
    for d in docs:
        r = prnd.random()
        if r < 0.55:
            d["xy"] = {"x": round(prnd.uniform(-1000, 1000), 3), "y": round(prnd.uniform(-50, 50), 4)}
        elif r < 0.75:
            d["xy"] = [[round(prnd.uniform(-10, 10), 2), round(prnd.uniform(1e5, 1e6), 1)], [prnd.randrange(-5, 5), prnd.randrange(-5, 5)]]
        elif r < 0.85:
            d["xy"] = "%s,%s" % (round(prnd.uniform(0, 1), 6), round(prnd.uniform(-3e7, 3e7), 2))
    return docs


def range_cases():
    c = []

    def add(name, aggs, query=None, typed_keys=False):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": typed_keys})

    add("range_histogram", {
        "r": {"histogram": {"field": "r", "interval": 5}, "aggs": {"a": {"avg": {"field": "n"}}}},
        "ro": {"histogram": {"field": "r", "interval": 7, "offset": 3, "order": {"_count": "desc"}}},
        "ir": {"histogram": {"field": "ir", "interval": 10, "keyed": True}},
        "fr": {"histogram": {"field": "fr", "interval": 2.5}},
        "dr": {"histogram": {"field": "dr", "interval": 25, "offset": 2.5, "min_doc_count": 3}},
        "tr": {"histogram": {"field": "tr", "interval": 604800000}},
    }, typed_keys=True)
    add("range_histogram_bounds", {
        "e": {"histogram": {"field": "r", "interval": 10, "min_doc_count": 0, "extended_bounds": {"min": -100, "max": 200}}},
        "h": {"histogram": {"field": "r", "interval": 5, "hard_bounds": {"min": 2, "max": 41}}},
        "hf": {"histogram": {"field": "dr", "interval": 50, "hard_bounds": {"min": -10.5, "max": 120}, "min_doc_count": 0}},
    })
    add("range_histogram_under_terms", {"k": {"terms": {"field": "k"}, "aggs": {
        "h": {"histogram": {"field": "r", "interval": 20}, "aggs": {"c": {"value_count": {"field": "n"}}}},
        "d": {"date_histogram": {"field": "tr", "calendar_interval": "quarter"}}}}})
    add("range_date_histogram", {
        "m": {"date_histogram": {"field": "tr", "calendar_interval": "month"}, "aggs": {"s": {"sum": {"field": "n"}}}},
        "w": {"date_histogram": {"field": "tr", "fixed_interval": "10d", "offset": "+2d", "format": "yyyy-MM-dd"}},
        "z": {"date_histogram": {"field": "tr", "calendar_interval": "week", "time_zone": "America/New_York", "min_doc_count": 5}},
        "o": {"date_histogram": {"field": "tr", "calendar_interval": "month", "offset": "+6h", "keyed": True}},
    })
    add("range_date_histogram_bounds", {
        "e": {"date_histogram": {"field": "tr", "calendar_interval": "month", "min_doc_count": 0, "extended_bounds": {"min": "2019-10-01", "max": "2021-06-01"}}},
        "h": {"date_histogram": {"field": "tr", "calendar_interval": "month", "hard_bounds": {"min": "2020-03-15", "max": "2020-08-01"}}},
        "hz": {"date_histogram": {"field": "tr", "calendar_interval": "day", "time_zone": "+05:30", "hard_bounds": {"min": 1590969600000, "max": 1591228800000}}},
    })
    add("range_date_histogram_query", {"m": {"date_histogram": {"field": "tr", "calendar_interval": "month"}}}, query={"term": {"k": "k1"}})
    add("range_metrics", {
        "c_r": {"cardinality": {"field": "r"}}, "c_tr": {"cardinality": {"field": "tr", "precision_threshold": 1}},
        "c_ipr": {"cardinality": {"field": "ipr", "precision_threshold": 5}}, "c_fr": {"cardinality": {"field": "fr", "precision_threshold": 3}},
        "c_dr": {"cardinality": {"field": "dr", "precision_threshold": 10}}, "c_open": {"cardinality": {"field": "open", "precision_threshold": 2}},
        "vc_r": {"value_count": {"field": "r"}}, "vc_ipr": {"value_count": {"field": "ipr"}}, "vc_ipr2": {"value_count": {"field": "ipr2"}}, "c_ipr2": {"cardinality": {"field": "ipr2"}}, "vc_open": {"value_count": {"field": "open"}},
        "m_r": {"missing": {"field": "r"}}, "m_ipr": {"missing": {"field": "ipr"}, "aggs": {"c": {"cardinality": {"field": "open"}}}},
    }, typed_keys=True)
    add("range_metrics_under_terms", {"k": {"terms": {"field": "k"}, "aggs": {"c": {"cardinality": {"field": "ir", "precision_threshold": 4}}, "v": {"value_count": {"field": "fr"}}}}})
    add("range_err_histogram_ip", {"h": {"histogram": {"field": "ipr", "interval": 5}}})
    add("range_err_date_histogram_long", {"h": {"date_histogram": {"field": "r", "fixed_interval": "1d"}}})
    add("range_err_date_histogram_ip", {"h": {"date_histogram": {"field": "ipr", "calendar_interval": "day"}}})
    add("range_err_missing", {"h": {"histogram": {"field": "r", "interval": 5, "missing": 3}}})
    add("range_err_missing_cardinality", {"c": {"cardinality": {"field": "tr", "missing": "2020-01-01"}}})
    add("range_err_format", {"h": {"histogram": {"field": "r", "interval": 5, "format": "0.0"}}})
    add("range_err_time_zone", {"h": {"histogram": {"field": "fr", "interval": 5}}, "d": {"date_histogram": {"field": "r", "fixed_interval": "1d", "time_zone": "+01:00"}}})
    add("range_err_terms", {"t": {"terms": {"field": "r"}}})
    add("range_err_avg", {"a": {"avg": {"field": "dr"}}})
    add("range_err_range", {"a": {"range": {"field": "r", "ranges": [{"to": 5}]}}})
    add("range_err_composite", {"c": {"composite": {"sources": [{"h": {"histogram": {"field": "r", "interval": 5}}}]}}})
    add("cartesian", {
        "b": {"cartesian_bounds": {"field": "xy"}}, "c": {"cartesian_centroid": {"field": "xy"}},
        "bm": {"cartesian_bounds": {"field": "xy", "missing": "-5000,7.5"}}, "cm": {"cartesian_centroid": {"field": "xy", "missing": "POINT (2.5 -1e6)"}},
        "bu": {"cartesian_bounds": {"field": "nope"}}, "cu": {"cartesian_centroid": {"field": "nope"}},
        "bum": {"cartesian_bounds": {"field": "nope", "missing": "1,2"}}, "cum": {"cartesian_centroid": {"field": "nope", "missing": "1.25,-2"}},
    }, typed_keys=True)
    add("cartesian_under_terms", {"k": {"terms": {"field": "k"}, "aggs": {"b": {"cartesian_bounds": {"field": "xy"}}, "c": {"cartesian_centroid": {"field": "xy"}}}},
                                  "mx": {"max_bucket": {"buckets_path": "k>c.x"}}, "mc": {"min_bucket": {"buckets_path": "k>c.count"}},
                                  "my": {"max_bucket": {"buckets_path": "k>b.top"}}, "ml": {"min_bucket": {"buckets_path": "k>b.left"}}})
    add("cartesian_err_bracket_path", {"k": {"terms": {"field": "k"}, "aggs": {"b": {"cartesian_bounds": {"field": "xy"}}}}, "ml": {"min_bucket": {"buckets_path": "k>b[bottom_right]"}}})
    add("cartesian_err_path", {"k": {"terms": {"field": "k"}, "aggs": {"b": {"cartesian_bounds": {"field": "xy"}}}}, "ml": {"min_bucket": {"buckets_path": "k>b.bottom_right.x"}}})
    add("cartesian_query", {"c": {"cartesian_centroid": {"field": "xy"}}, "b": {"cartesian_bounds": {"field": "xy"}}}, query={"term": {"k": "k2"}})
    add("cartesian_err_value_count", {"v": {"value_count": {"field": "xy"}}})
    add("cartesian_err_cardinality", {"v": {"cardinality": {"field": "xy"}}})
    add("cartesian_err_missing_agg", {"v": {"missing": {"field": "xy"}}})
    add("cartesian_err_terms", {"v": {"terms": {"field": "xy"}}})
    add("cartesian_err_keyword", {"v": {"cartesian_bounds": {"field": "k"}}})
    add("cartesian_err_range_field", {"v": {"cartesian_centroid": {"field": "ipr"}}})
    add("cartesian_err_missing_coords", {"v": {"cartesian_centroid": {"field": "xy", "missing": "1,2,3,4"}}})
    add("cartesian_err_missing_symbol", {"v": {"cartesian_centroid": {"field": "xy", "missing": "abc"}}})
    add("cartesian_err_missing_z", {"v": {"cartesian_bounds": {"field": "xy", "missing": "1,2,3"}}})
    add("cartesian_err_missing_object", {"v": {"cartesian_bounds": {"field": "xy", "missing": {"x": 1, "y": 2}}}})
    add("cartesian_err_wrap", {"v": {"cartesian_bounds": {"field": "xy", "wrap_longitude": True}}})
    add("cartesian_err_script", {"v": {"cartesian_bounds": {"field": "xy", "script": "1"}}})
    add("cartesian_err_subs", {"v": {"cartesian_centroid": {"field": "xy"}, "aggs": {"m": {"max": {"field": "n"}}}}})
    add("range_err_percentiles", {"p": {"percentiles": {"field": "fr"}}})
    return c


def generate(suite, mapping, docs, cases, shards, settings=None):
    index = "aggs-fx-" + suite
    call("DELETE", "/" + index)
    tsdb = settings is not None and settings.get("index.mode") == "time_series"
    st = {"number_of_shards": shards, "number_of_replicas": 0}
    st.update(settings or {})
    r = call("PUT", "/" + index, {"settings": st, "mappings": mapping})
    assert r.get("acknowledged"), r
    lines = []
    for i, d in enumerate(docs):
        if tsdb:
            # Time series indices make their own ids: docs carry their position in `seq`.
            lines.append(json.dumps({"create": {}}))
            lines.append(json.dumps(dict(d, seq=i)))
            continue
        action = {"_id": str(i)}
        if "_routing" in d:
            d = dict(d)
            action["routing"] = d.pop("_routing")
        lines.append(json.dumps({"index": action}))
        lines.append(json.dumps(d))
    r = call("POST", "/" + index + "/_bulk?refresh=true", "\n".join(lines) + "\n", ndjson=True)
    assert not r.get("errors"), [i for i in r["items"] if "error" in list(i.values())[0]][:3]
    call("POST", "/" + index + "/_forcemerge?max_num_segments=1")
    call("POST", "/" + index + "/_refresh")
    # Shard of every doc, in each shard's doc order.
    placement = []
    for s in range(shards):
        if tsdb:
            r = call("POST", "/" + index + "/_search?preference=_shards:%d" % s,
                     {"size": 10000, "sort": ["_doc"], "_source": False, "docvalue_fields": ["seq"]})
            placement.append([h["fields"]["seq"][0] for h in r["hits"]["hits"]])
        else:
            r = call("POST", "/" + index + "/_search?preference=_shards:%d" % s,
                     {"size": 10000, "sort": ["_doc"], "_source": False})
            placement.append([int(h["_id"]) for h in r["hits"]["hits"]])
    out_cases = []
    for c in cases:
        q = "/_search?size=0" + ("&typed_keys=true" if c["typed_keys"] else "")
        r = call("POST", "/" + index + q, c["body"])
        entry = dict(c)
        if "error" in r:
            entry["error"] = r["error"]
        else:
            entry["response"] = r.get("aggregations")
        out_cases.append(entry)
    call("DELETE", "/" + index)
    docs = [{k: v for k, v in d.items() if k != "_routing"} for d in docs]
    fixture = {"suite": suite, "shards": shards, "mapping": mapping, "docs": docs, "placement": placement, "cases": out_cases}
    if tsdb:
        fixture["docs"] = [dict(d, seq=i) for i, d in enumerate(docs)]
    with open(os.path.join(HERE, "aggs-" + suite + ".json"), "w") as f:
        json.dump(fixture, f, indent=1)
        f.write("\n")
    print(suite, len(out_cases), "cases")


SUITES = {
    "main1": lambda: generate("main1", MAIN_MAPPING, main_docs(400, 42), main_cases(), 1),
    "main3": lambda: generate("main3", MAIN_MAPPING, main_docs(400, 42), main_cases(), 3),
    "join2": lambda: generate("join2", JOIN_MAPPING, join_docs(7), join_cases(), 2),
    "bulk3": lambda: generate("bulk3", BULK_MAPPING, bulk_docs(12000, 11), bulk_cases(), 3),
    "range2": lambda: generate("range2", RANGE_MAPPING, range_docs(500, 5), range_cases(), 2),
    "pre2": lambda: generate("pre2", PRE_MAPPING, pre_docs(500, 9), pre_cases(), 2),
    "shape2": lambda: generate("shape2", SHAPE_MAPPING, shape_docs(300, 17), shape_cases(), 2),
    "exp2": lambda: generate("exp2", EXP_MAPPING, exp_docs(400, 13), exp_cases(), 2),
    "ts2": lambda: generate("ts2", TS_MAPPING, ts_docs(700, 3), ts_cases(), 2, TS_SETTINGS),
}

if __name__ == "__main__":
    names = sys.argv[1:] or list(SUITES)
    for n in names:
        SUITES[n]()
