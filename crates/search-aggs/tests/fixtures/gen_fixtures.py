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
    for attempt in range(3):
        req = urllib.request.Request(ES + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req) as r:
                return json.loads(r.read().decode())
        except urllib.error.HTTPError as e:
            return json.loads(e.read().decode())
        except (ConnectionError, urllib.error.URLError) as e:
            print("retrying", method, path, e, file=sys.stderr)
            if attempt == 2:
                raise


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
    add("time_series_err_composite_parent", {"c": {"composite": {"sources": [{"h": {"terms": {"field": "host"}}}]}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_err_composite_sibling", {"ts": {"time_series": {}}, "c": {"composite": {"sources": [{"h": {"terms": {"field": "host"}}}]}}})
    add("time_series_err_global", {"g": {"global": {}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_err_rare_terms", {"r": {"rare_terms": {"field": "host"}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_rare_terms_sibling", {"ts": {"time_series": {}}, "r": {"rare_terms": {"field": "host"}}})
    add("time_series_err_rare_terms_subs_sibling", {"ts": {"time_series": {}}, "r": {"rare_terms": {"field": "host"}, "aggs": {"m": {"max": {"field": "cpu"}}}}})
    add("time_series_err_sampler", {"s": {"sampler": {}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_err_diversified", {"s": {"diversified_sampler": {"field": "host"}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_err_variable_width", {"v": {"variable_width_histogram": {"field": "cpu"}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_err_auto_date", {"a": {"auto_date_histogram": {"field": "@timestamp"}, "aggs": {"ts": {"time_series": {}}}}})
    add("time_series_breadth_first_terms", {"t": {"terms": {"field": "host", "collect_mode": "breadth_first"}, "aggs": {"ts": {"time_series": {}}}}})
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


LIC_MAPPING = {"properties": {
    "k": {"type": "keyword"}, "n": {"type": "long"}, "t": {"type": "date"}, "g": {"type": "geo_point"},
    "d": {"type": "double"},
}}


def lic_docs(n, seed):
    """Tracks for the license-gated aggregations (`geo_line`, `geohex_grid`): points that wander
    around a few centres, clustered near the poles and the antimeridian too, with ties in `t`
    and documents missing the point or the sort value."""
    rnd = random.Random(seed)
    centres = [(47.5, 19.0), (-33.9, 151.2), (64.1, -21.9), (0.0, 179.95), (89.2, 10.0), (-88.5, -60.0), (35.7, 139.7)]
    docs = []
    for i in range(n):
        d = {"k": KEYWORDS[min(int(rnd.expovariate(0.5)), 6)], "n": rnd.randrange(100), "d": round(rnd.uniform(-5, 5), 3)}
        if rnd.random() < 0.93:
            la, lo = centres[rnd.randrange(len(centres))]
            lat = max(-90.0, min(90.0, la + rnd.gauss(0, 0.6)))
            lon = lo + rnd.gauss(0, 0.6)
            lon = (lon + 180.0) % 360.0 - 180.0
            d["g"] = {"lat": round(lat, 6), "lon": round(lon, 6)}
        if rnd.random() < 0.95:
            d["t"] = 1600000000000 + rnd.randrange(400) * 60000
        docs.append(d)
    return docs


def lic_cases():
    c = []

    def add(name, aggs, query=None):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": False})

    gl = {"point": {"field": "g"}, "sort": {"field": "t"}}
    add("geo_line", {"l": {"geo_line": gl}, "inc": {"geo_line": dict(gl, include_sort=True)},
                     "desc": {"geo_line": dict(gl, sort_order="DESC", include_sort=True, size=20)},
                     "small": {"geo_line": dict(gl, size=5)}, "by_n": {"geo_line": {"point": {"field": "g"}, "sort": {"field": "n"}, "size": 30, "include_sort": True}},
                     "by_d": {"geo_line": {"point": {"field": "g"}, "sort": {"field": "d"}, "size": 15, "sort_order": "desc"}}})
    add("geo_line_under_terms", {"t": {"terms": {"field": "k", "size": 4}, "aggs": {"l": {"geo_line": dict(gl, size=12, include_sort=True)}}}})
    add("geo_line_under_histogram", {"h": {"histogram": {"field": "n", "interval": 20}, "aggs": {"l": {"geo_line": dict(gl, size=8)}}}})
    add("geo_line_query", {"l": {"geo_line": dict(gl, size=50)}}, query={"term": {"k": "k01"}})
    add("geo_line_empty", {"l": {"geo_line": gl}}, query={"term": {"k": "zzz"}})
    add("geo_line_err_size", {"l": {"geo_line": dict(gl, size=0)}})
    add("geo_line_err_big", {"l": {"geo_line": dict(gl, size=100001)}})
    add("geo_line_err_no_sort", {"l": {"geo_line": {"point": {"field": "g"}}}})
    add("geo_line_err_point_type", {"l": {"geo_line": {"point": {"field": "n"}, "sort": {"field": "t"}}}})
    add("geo_line_err_sort_type", {"l": {"geo_line": {"point": {"field": "g"}, "sort": {"field": "k"}}}})
    add("geo_line_err_order", {"l": {"geo_line": dict(gl, sort_order="up")}})
    add("geo_line_unmapped_point", {"l": {"geo_line": {"point": {"field": "nope"}, "sort": {"field": "t"}}}})
    add("geo_line_unmapped_sort", {"l": {"geo_line": {"point": {"field": "g"}, "sort": {"field": "nope"}}}})
    add("geohex_grid", {"h0": {"geohex_grid": {"field": "g", "precision": 0}}, "h2": {"geohex_grid": {"field": "g", "precision": 2}},
                        "h": {"geohex_grid": {"field": "g"}}, "h7": {"geohex_grid": {"field": "g", "precision": 7, "size": 15}},
                        "h15": {"geohex_grid": {"field": "g", "precision": 15, "size": 5, "shard_size": 7}}})
    add("geohex_grid_bounds", {"b": {"geohex_grid": {"field": "g", "precision": 3, "bounds": {"top_left": {"lat": 70, "lon": -30}, "bottom_right": {"lat": 30, "lon": 30}}}},
                               "anti": {"geohex_grid": {"field": "g", "precision": 4, "bounds": {"top_left": "10, 170", "bottom_right": "-10, -170"}}},
                               "pole": {"geohex_grid": {"field": "g", "precision": 1, "bounds": {"top_right": "90, 180", "bottom_left": "80, -180"}}}})
    add("geohex_grid_subs", {"h": {"geohex_grid": {"field": "g", "precision": 1}, "aggs": {"a": {"avg": {"field": "n"}}, "t": {"terms": {"field": "k", "size": 2}}}}})
    add("geohex_grid_under_terms", {"t": {"terms": {"field": "k"}, "aggs": {"h": {"geohex_grid": {"field": "g", "precision": 2, "size": 3}}}}})
    add("geohex_grid_err_precision", {"h": {"geohex_grid": {"field": "g", "precision": 16}}})
    add("geohex_grid_err_negative", {"h": {"geohex_grid": {"field": "g", "precision": -1}}})
    add("geohex_grid_err_type", {"h": {"geohex_grid": {"field": "n"}}})
    add("geohex_grid_unmapped", {"h": {"geohex_grid": {"field": "nope"}}})
    add("geohex_grid_pipeline", {"h": {"geohex_grid": {"field": "g", "precision": 1}, "aggs": {"m": {"max": {"field": "n"}}}}, "mb": {"max_bucket": {"buckets_path": "h>m"}}})
    return c


TSG_MAPPING = {"properties": {
    "@timestamp": {"type": "date"}, "host": {"type": "keyword", "time_series_dimension": True},
    "loc": {"type": "geo_point"}, "bytes": {"type": "long", "time_series_metric": "counter"},
    "seq": {"type": "long"}, "cpu": {"type": "double", "time_series_metric": "gauge"},
}}
TSG_SETTINGS = {"index.mode": "time_series", "index.routing_path": ["host"],
                "index.time_series.start_time": "2020-01-01T00:00:00Z", "index.time_series.end_time": "2030-01-01T00:00:00Z"}


def tsg_docs(n, seed):
    """Vehicles (series) reporting positions over time; some samples lack the position or the
    counter (a counter missing from whole series too)."""
    rnd = random.Random(seed)
    docs = []
    t0 = 1609459200000
    hosts = ["v%02d" % i for i in range(14)]
    state = {h: [rnd.uniform(-60, 60), rnd.uniform(-170, 170), rnd.randrange(1000)] for h in hosts}
    seen = set()
    while len(docs) < n:
        h = hosts[min(int(rnd.expovariate(0.2)), len(hosts) - 1)]
        t = t0 + rnd.randrange(4 * 3600) * 1000
        if (h, t) in seen:
            continue
        seen.add((h, t))
        st = state[h]
        st[0] = max(-89.0, min(89.0, st[0] + rnd.gauss(0, 0.05)))
        st[1] = max(-179.0, min(179.0, st[1] + rnd.gauss(0, 0.05)))
        d = {"host": h, "@timestamp": t, "cpu": round(rnd.uniform(0, 100), 2)}
        if rnd.random() < 0.9:
            d["loc"] = {"lat": round(st[0], 6), "lon": round(st[1], 6)}
        if h not in ("v03", "v07") and rnd.random() < 0.85:
            d["bytes"] = 0
        docs.append(d)
    crnd = random.Random(seed + 3)
    for h in hosts:
        value = crnd.randrange(1000)
        for d in sorted((d for d in docs if d["host"] == h), key=lambda d: d["@timestamp"]):
            if "bytes" in d:
                value = crnd.randrange(50) if crnd.random() < 0.05 else value + crnd.randrange(500)
                d["bytes"] = value
    return docs


def tsg_cases():
    c = []

    def add(name, aggs, query=None):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": False})

    add("ts_geo_line", {"ts": {"time_series": {"keyed": False}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}}},
                                                                         "s": {"geo_line": {"point": {"field": "loc"}, "size": 10}},
                                                                         "i": {"geo_line": {"point": {"field": "loc"}, "size": 25, "include_sort": True}}}}})
    add("ts_geo_line_keyed", {"ts": {"time_series": {}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}, "size": 4}}}}})
    add("ts_geo_line_sort", {"ts": {"time_series": {"keyed": False, "size": 3}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}, "sort": {"field": "cpu"}, "size": 6}}}}})
    add("ts_geo_line_under_histogram", {"d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "1h"}, "aggs": {"ts": {"time_series": {"keyed": False, "size": 2}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}, "size": 5}}}}}}})
    add("ts_geo_line_err_no_ts", {"l": {"geo_line": {"point": {"field": "loc"}}}})
    add("ts_geo_line_top_level_sorted", {"l": {"geo_line": {"point": {"field": "loc"}, "sort": {"field": "@timestamp"}, "size": 40}}})
    add("ts_geo_line_desc", {"ts": {"time_series": {"keyed": False, "size": 4}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}, "size": 7, "sort_order": "desc", "include_sort": True}}}}})
    add("ts_geo_line_size1", {"ts": {"time_series": {"keyed": False}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}, "size": 1}}}}})
    add("ts_geo_line_size2", {"ts": {"time_series": {"keyed": False}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}, "size": 2, "include_sort": True}}}}})
    add("ts_geo_line_size3", {"ts": {"time_series": {"keyed": False}, "aggs": {"l": {"geo_line": {"point": {"field": "loc"}, "size": 3, "include_sort": True}}}}})
    add("ts_geo_line_beside", {"ts": {"time_series": {"size": 1}}, "l": {"geo_line": {"point": {"field": "loc"}, "size": 30, "include_sort": True}}})
    add("ts_counter_missing", {"ts": {"time_series": {"keyed": False}, "aggs": {"r": {"rate": {"field": "bytes"}}, "v": {"value_count": {"field": "bytes"}}}}})
    add("ts_counter_missing_hourly", {"ts": {"time_series": {"keyed": False}, "aggs": {"d": {"date_histogram": {"field": "@timestamp", "fixed_interval": "1h"}, "aggs": {"r": {"rate": {"field": "bytes", "unit": "minute"}}}}}}})
    add("ts_counter_missing_query", {"ts": {"time_series": {"keyed": False}, "aggs": {"r": {"rate": {"field": "bytes"}}}}}, query={"range": {"cpu": {"lt": 40}}})
    return c


FIS_MAPPING = {"properties": {
    "k": {"type": "keyword"}, "c": {"type": "keyword"}, "n": {"type": "long"}, "ip": {"type": "ip"},
    "b": {"type": "boolean"}, "t": {"type": "date"}, "d": {"type": "double"}, "txt": {"type": "text"},
    "g": {"type": "keyword"},
}}


def fis_docs(n, seed):
    """Shopping-basket-like transactions: products (`k`, multi-valued) bought together in
    correlated groups, a category `c`, plus numeric, ip, boolean and date items. Item
    probabilities differ enough that item counts rarely tie."""
    rnd = random.Random(seed)
    products = ["p%02d" % i for i in range(24)]
    weights = [0.9 ** i for i in range(24)]
    bundles = [["p00", "p03", "p07"], ["p01", "p02"], ["p04", "p05", "p06", "p09"], ["p00", "p11"]]
    docs = []
    for i in range(n):
        d = {"g": "g%d" % (i % 3)}
        ks = set()
        for _ in range(rnd.randrange(0, 4)):
            ks.add(rnd.choices(products, weights)[0])
        if rnd.random() < 0.45:
            ks.update(bundles[min(int(rnd.expovariate(0.9)), 3)])
        if ks:
            d["k"] = sorted(ks) if len(ks) > 1 else next(iter(ks))
        if rnd.random() < 0.8:
            d["c"] = rnd.choices(["food", "tools", "toys", "books", "misc"], [8, 5, 3, 2, 1])[0]
        if rnd.random() < 0.7:
            d["n"] = rnd.choices([1, 2, 3, 5, 8, 13], [10, 7, 5, 3, 2, 1])[0]
        if rnd.random() < 0.6:
            d["ip"] = rnd.choices(["10.0.0.1", "10.0.0.2", "192.168.0.7", "::1"], [6, 3, 2, 1])[0]
        if rnd.random() < 0.75:
            d["b"] = rnd.random() < 0.7
        if rnd.random() < 0.5:
            d["t"] = rnd.choices([1600000000000, 1600086400000, 1600172800000], [5, 3, 1])[0]
        if rnd.random() < 0.5:
            d["d"] = rnd.choices([1.25, 2.75, -3.5], [4, 2, 1])[0]
        d["txt"] = "hello world"
        docs.append(d)
    return docs


def fis_cases():
    c = []

    def add(name, aggs, query=None):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": False})

    fi = "frequent_item_sets"
    add("fis_basic", {"f": {fi: {"fields": [{"field": "k"}, {"field": "c"}]}}})
    add("fis_support", {"f": {fi: {"fields": [{"field": "k"}, {"field": "c"}], "minimum_support": 0.05, "size": 25}},
                        "g": {fi: {"fields": [{"field": "k"}], "minimum_support": 0.2}}})
    add("fis_set_size", {"s2": {fi: {"fields": [{"field": "k"}, {"field": "c"}], "minimum_set_size": 2, "minimum_support": 0.03}},
                         "s3": {fi: {"fields": [{"field": "k"}], "minimum_set_size": 3, "minimum_support": 0.02, "size": 7}}})
    add("fis_types", {"f": {fi: {"fields": [{"field": "n"}, {"field": "ip"}, {"field": "b"}, {"field": "t"}, {"field": "d"}], "minimum_support": 0.05, "size": 30}}})
    add("fis_mixed", {"f": {fi: {"fields": [{"field": "k"}, {"field": "n"}, {"field": "c"}, {"field": "b"}], "minimum_support": 0.04, "size": 40, "minimum_set_size": 2}}})
    add("fis_size_small", {"f": {fi: {"fields": [{"field": "k"}, {"field": "c"}, {"field": "n"}], "size": 3, "minimum_support": 0.02}}})
    add("fis_filter", {"f": {fi: {"fields": [{"field": "k"}, {"field": "c"}], "filter": {"term": {"b": True}}, "minimum_support": 0.05}}})
    add("fis_include", {"f": {fi: {"fields": [{"field": "k", "include": "p0[0-5]"}, {"field": "c", "exclude": ["misc", "books"]}], "minimum_support": 0.03}},
                        "n": {fi: {"fields": [{"field": "n", "include": [1, 2, 3]}, {"field": "k", "exclude": "p0.*"}], "minimum_support": 0.03}}})
    add("fis_partition", {"f": {fi: {"fields": [{"field": "k", "include": {"partition": 0, "num_partitions": 2}}], "minimum_support": 0.03}}})
    add("fis_hint", {"m": {fi: {"fields": [{"field": "k"}, {"field": "c"}], "execution_hint": "map", "minimum_support": 0.05}},
                     "o": {fi: {"fields": [{"field": "k"}, {"field": "c"}], "execution_hint": "global_ordinals", "minimum_support": 0.05}}})
    add("fis_missing", {"f": {fi: {"fields": [{"field": "c", "missing": "none"}, {"field": "n", "missing": 0}], "minimum_support": 0.05}}})
    # Equal item counts order by Elasticsearch's per-JVM hash seed: supports here keep the
    # items involved distinct.
    add("fis_query", {"f": {fi: {"fields": [{"field": "k"}, {"field": "c"}], "minimum_support": 0.2}}}, query={"term": {"c": "food"}})
    add("random_sampler_fis", {"s": {"random_sampler": {"probability": 0.5, "seed": 7, "shard_seed": 3}, "aggs": {"f": {fi: {"fields": [{"field": "c"}], "minimum_support": 0.1}}}}})
    add("fis_typed", {"f": {fi: {"fields": [{"field": "c"}], "minimum_support": 0.2}}, "m": {"max": {"field": "n"}}})
    add("fis_empty", {"f": {fi: {"fields": [{"field": "k"}]}}}, query={"term": {"c": "zzz"}})
    add("fis_same_field_twice", {"f": {fi: {"fields": [{"field": "c"}, {"field": "c"}], "minimum_support": 0.1}}})
    add("fis_under_terms", {"t": {"terms": {"field": "g"}, "aggs": {"f": {fi: {"fields": [{"field": "k"}, {"field": "c"}], "minimum_support": 0.1, "size": 4}}}}})
    add("fis_under_terms_ordered", {"t": {"terms": {"field": "g", "order": {"_key": "desc"}}, "aggs": {"f": {fi: {"fields": [{"field": "c"}], "minimum_support": 0.1, "size": 3}}}}})
    add("fis_under_filter", {"x": {"filter": {"term": {"c": "tools"}}, "aggs": {"f": {fi: {"fields": [{"field": "k"}], "minimum_support": 0.1}}}}})
    add("fis_deprecated_name", {"f": {"frequent_items": {"fields": [{"field": "c"}], "minimum_support": 0.2}}})
    add("fis_err_support_zero", {"f": {fi: {"fields": [{"field": "c"}], "minimum_support": 0}}})
    add("fis_err_support_big", {"f": {fi: {"fields": [{"field": "c"}], "minimum_support": 1.5}}})
    add("fis_err_set_size", {"f": {fi: {"fields": [{"field": "c"}], "minimum_set_size": 0}}})
    add("fis_err_size", {"f": {fi: {"fields": [{"field": "c"}], "size": 0}}})
    add("fis_err_hint", {"f": {fi: {"fields": [{"field": "c"}], "execution_hint": "foo"}}})
    add("fis_err_no_fields", {"f": {fi: {"minimum_support": 0.1}}})
    add("fis_err_unknown", {"f": {fi: {"fields": [{"field": "c"}], "foo": 1}}})
    add("fis_err_field_unknown", {"f": {fi: {"fields": [{"field": "c", "script": "1"}]}}})
    add("fis_err_unmapped", {"f": {fi: {"fields": [{"field": "nope"}]}}})
    add("fis_err_unmapped_second", {"f": {fi: {"fields": [{"field": "c"}, {"field": "nope"}]}}})
    add("fis_err_text", {"f": {fi: {"fields": [{"field": "txt"}]}}})
    add("fis_err_subs", {"f": {fi: {"fields": [{"field": "c"}]}, "aggs": {"m": {"max": {"field": "n"}}}}})
    add("fis_err_fields_not_array", {"f": {fi: {"fields": {"field": "c"}}}})
    add("fis_err_pipeline", {"f": {fi: {"fields": [{"field": "c"}]}}, "m": {"max_bucket": {"buckets_path": "f>_count"}}})
    return c


CAT_MAPPING = {"properties": {
    "msg": {"type": "text"}, "kmsg": {"type": "keyword"}, "g": {"type": "keyword"}, "n": {"type": "long"},
    "other": {"type": "text"},
}}

CAT_TEMPLATES = [
    "Node {n} stopping", "Node {n} starting", "User {u} logging on", "User {u} logging off",
    "Failed to connect to {ip}:{port} after {n} attempts", "Connection from {ip} accepted",
    "Disk usage at {n}% on /dev/sd{c}", "Request GET /api/v1/items/{n} completed in {n}ms",
    "Request POST /api/v1/orders completed in {n}ms with status 201",
    "Scheduled job {u}-{n} finished on Monday at {n}:00 UTC",
    "error: cannot open file C:\\logs\\app{n}.log: access denied",
    "Cache miss for key session_{hex} in region eu-west-{n}",
    "Heartbeat received from worker {u}@host{n}.example.com",
    "Transaction {hex} committed",
    "Transaction {hex} rolled back due to deadlock on table {t}",
    "Payment of {n}.{n} EUR authorised for customer {u}",
    "Starting service {t} version {n}.{n}.{n}",
    "Shutting down service {t} gracefully, {n} requests in flight",
    "Out of memory: killed process {n} ({t}) score {n}",
    "GC pause young generation {n}ms, heap {n}MB->{n}MB",
]
CAT_USERS = ["Foo", "alice", "bob", "carol", "dave", "eve", "mallory"]
CAT_TABLES = ["orders", "users", "items", "payments"]


def cat_docs(n, seed):
    """Log messages from templates with variable parts (numbers, ips, hex ids, paths), some
    multi-line (stack traces), some multi-valued, some without the field."""
    rnd = random.Random(seed)
    weights = [1.0 / (1 + i * 0.35) for i in range(len(CAT_TEMPLATES))]
    docs = []
    for i in range(n):
        def msg():
            t = rnd.choices(CAT_TEMPLATES, weights)[0]
            return t.format(n=rnd.randrange(1, 500), u=rnd.choice(CAT_USERS), ip="10.%d.%d.%d" % (rnd.randrange(256), rnd.randrange(256), rnd.randrange(256)),
                            port=rnd.choice([80, 443, 5432, 9200]), c=rnd.choice("abc"), hex="%08x" % rnd.getrandbits(32), t=rnd.choice(CAT_TABLES))
        d = {"g": "g%d" % (i % 3), "n": rnd.randrange(10)}
        r = rnd.random()
        if r < 0.85:
            m = msg()
            if rnd.random() < 0.08:
                m = "\n  \n" + m + "\n\tat com.example.Foo.bar(Foo.java:%d)\n\tat com.example.Main" % rnd.randrange(100)
            d["msg"] = m
            d["kmsg"] = m
        elif r < 0.92:
            d["msg"] = [msg(), msg()]
        elif r < 0.95:
            d["msg"] = "12345 67890 ---"
        if rnd.random() < 0.3:
            d["other"] = msg()
        docs.append(d)
    return docs


def cat_cases():
    c = []

    def add(name, aggs, query=None):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": False})

    ct = "categorize_text"
    add("cat_basic", {"c": {ct: {"field": "msg"}}})
    add("cat_size", {"c": {ct: {"field": "msg", "size": 25}}, "s": {ct: {"field": "msg", "size": 3, "shard_size": 4}}})
    add("cat_threshold", {"t11": {ct: {"field": "msg", "similarity_threshold": 11, "size": 20}}, "t50": {ct: {"field": "msg", "similarity_threshold": 50, "size": 20}},
                          "t95": {ct: {"field": "msg", "similarity_threshold": 95, "size": 30}}, "t100": {ct: {"field": "msg", "similarity_threshold": 100, "size": 30}}})
    add("cat_min_doc_count", {"c": {ct: {"field": "msg", "min_doc_count": 20, "size": 50}}, "z": {ct: {"field": "msg", "min_doc_count": 0, "size": 50}}})
    add("cat_keyword", {"c": {ct: {"field": "kmsg", "size": 15}}})
    add("cat_subs", {"c": {ct: {"field": "msg", "size": 6}, "aggs": {"g": {"terms": {"field": "g"}}, "a": {"avg": {"field": "n"}}}}})
    add("cat_under_terms", {"t": {"terms": {"field": "g"}, "aggs": {"c": {ct: {"field": "msg", "size": 4}}}}})
    add("cat_query", {"c": {ct: {"field": "msg", "size": 10}}}, query={"range": {"n": {"gte": 6}}})
    add("cat_empty", {"c": {ct: {"field": "msg"}}}, query={"term": {"g": "zzz"}})
    add("cat_unmapped", {"c": {ct: {"field": "nope"}}})
    add("cat_other", {"c": {ct: {"field": "other", "size": 30}}})
    add("cat_pipeline", {"c": {ct: {"field": "msg", "size": 5}, "aggs": {"a": {"avg": {"field": "n"}}}}, "m": {"max_bucket": {"buckets_path": "c>a"}},
                         "b": {"avg_bucket": {"buckets_path": "c>_count"}}})
    add("cat_deprecated_params", {"c": {ct: {"field": "msg", "max_unique_tokens": 10, "max_matched_tokens": 5, "size": 5}}})
    add("cat_err_threshold_zero", {"c": {ct: {"field": "msg", "similarity_threshold": 0}}})
    add("cat_err_threshold_big", {"c": {ct: {"field": "msg", "similarity_threshold": 101}}})
    add("cat_err_size", {"c": {ct: {"field": "msg", "size": 0}}})
    add("cat_err_shard_size", {"c": {ct: {"field": "msg", "shard_size": -1}}})
    add("cat_err_min_doc_count", {"c": {ct: {"field": "msg", "min_doc_count": -1}}})
    add("cat_err_shard_min_doc_count", {"c": {ct: {"field": "msg", "shard_min_doc_count": -2}}})
    add("cat_err_filters_and_analyzer", {"c": {ct: {"field": "msg", "categorization_filters": ["foo"], "categorization_analyzer": "english"}}})
    add("cat_err_analyzer_and_filters", {"c": {ct: {"field": "msg", "categorization_analyzer": "english", "categorization_filters": ["foo"]}}})
    add("cat_err_filters_dup", {"c": {ct: {"field": "msg", "categorization_filters": ["a", "a"]}}})
    add("cat_err_filters_empty", {"c": {ct: {"field": "msg", "categorization_filters": ["a", ""]}}})
    add("cat_err_filters_regex", {"c": {ct: {"field": "msg", "categorization_filters": ["(unclosed"]}}})
    add("cat_err_numeric", {"c": {ct: {"field": "n"}}})
    add("cat_err_no_field", {"c": {ct: {"size": 3}}})
    add("cat_err_unknown", {"c": {ct: {"field": "msg", "foo": 1}}})
    return c


CATR_MAPPING = {"properties": {"msg": {"type": "text"}, "r": {"type": "keyword"}}}
# Dictionary words of aggs-cat-dict.txt (verbs, nouns, others) and tokens that aren't words.
CATR_WORDS = ["failed", "connect", "completed", "cannot", "get", "killed", "received", "rolled", "starting", "attempts", "committed",
              "cache", "connection", "customer", "disk", "error", "file", "heap", "job", "node", "service", "session", "table", "user",
              "after", "from", "to", "with", "on", "off", "accepted", "open", "gracefully", "out"]
CATR_OTHER = ["db01", "x", "Zq", "alpha-3", "srv_9", "k8s", "node7", "eu-1", "qq", "w", "Rx", "p9z"]


def catr_docs(n, seed):
    """Random near-duplicate messages: a few base token sequences mutated by substitutions,
    insertions and deletions, so categories merge, split and lose common tokens; each doc's
    round `r` is a separate categorization problem."""
    rnd = random.Random(seed)
    docs = []
    for rid in range(12):
        bases = [[rnd.choice(CATR_WORDS + CATR_OTHER) for _ in range(rnd.randrange(2, 9))] for _ in range(rnd.randrange(2, 6))]
        for _ in range(rnd.randrange(20, 70)):
            t = list(rnd.choice(bases))
            for _ in range(rnd.randrange(0, 4)):
                op = rnd.random()
                if op < 0.4 and t:
                    t[rnd.randrange(len(t))] = rnd.choice(CATR_WORDS + CATR_OTHER)
                elif op < 0.7:
                    t.insert(rnd.randrange(len(t) + 1), rnd.choice(CATR_WORDS + CATR_OTHER))
                elif t:
                    del t[rnd.randrange(len(t))]
            if not t:
                t = ["x"]
            docs.append({"r": "r%02d" % rid, "msg": " ".join(t) + " " * rnd.randrange(0, 3)})
    return docs


def catr_cases():
    c = []
    for th in [20, 45, 70, 85, 100]:
        aggs = {"t": {"terms": {"field": "r", "size": 20}, "aggs": {"c": {"categorize_text": {"field": "msg", "similarity_threshold": th, "size": 60}}}}}
        c.append({"name": "catr_%d" % th, "body": json.dumps({"aggs": aggs}, separators=(",", ":")), "typed_keys": False})
    return c



def core_err_cases():
    """Parse and validation errors (and edge cases next to them) of the core aggregations."""
    c = []

    def add(name, aggs, query=None, typed_keys=False, raw=None):
        if raw is not None:
            body = raw
        else:
            body = {"aggs": aggs}
            if query is not None:
                body["query"] = query
            body = json.dumps(body, separators=(",", ":"))
        c.append({"name": name, "body": body, "typed_keys": typed_keys})

    # Structure.
    add("struct_unknown_type", {"a": {"foo": {"field": "n"}}})
    add("struct_empty", {"a": {}})
    add("struct_two_types", {"a": {"terms": {"field": "k"}, "avg": {"field": "n"}}})
    add("struct_metric_subs", {"a": {"avg": {"field": "n"}, "aggs": {"b": {"max": {"field": "n"}}}}})
    add("struct_bad_name_bracket", {"a[b": {"avg": {"field": "n"}}})
    add("struct_bad_name_gt", {"a>b": {"avg": {"field": "n"}}})
    add("struct_bad_name_dot", {"a.b": {"avg": {"field": "n"}}})
    add("struct_meta_not_object", {"a": {"avg": {"field": "n"}, "meta": 5}})
    add("struct_meta", {"a": {"avg": {"field": "n"}, "meta": {"x": [1, {"y": "z"}], "n": None}}, "b": {"terms": {"field": "k", "size": 1}, "meta": {"q": 1}}})
    add("struct_aggs_array", {"a": {"terms": {"field": "k"}, "aggs": []}})
    add("struct_aggs_twice", {"a": {"terms": {"field": "k"}, "aggs": {"x": {"avg": {"field": "n"}}}, "aggregations": {"y": {"avg": {"field": "n"}}}}})
    add("struct_type_not_object", {"a": {"avg": 5}})
    add("struct_agg_not_object", {"a": 5})
    add("struct_aggs_not_object", None, raw='{"aggs":5}')
    add("struct_unknown_field", {"a": {"avg": {"field": "n", "foo": 1}}})
    add("struct_unknown_terms_field", {"a": {"terms": {"field": "k", "foo": 1}}})
    add("struct_field_array", {"a": {"avg": {"field": ["n"]}}})
    add("struct_no_field", {"a": {"avg": {}}})
    add("struct_no_field_terms", {"a": {"terms": {}}})
    add("struct_null_field", {"a": {"max": {"field": None}}})
    add("struct_global_under_terms", {"t": {"terms": {"field": "k"}, "aggs": {"g": {"global": {}}}}})
    add("struct_global_with_body", {"g": {"global": {"x": 1}}})
    # Metrics.
    add("metric_stats_keyword", {"a": {"stats": {"field": "k"}}})
    add("metric_sum_keyword", {"a": {"sum": {"field": "k"}}})
    add("metric_avg_geo", {"a": {"avg": {"field": "g"}}})
    add("metric_min_ip", {"a": {"min": {"field": "ip"}}})
    add("metric_max_text", {"a": {"max": {"field": "txt"}}})
    add("metric_missing_bad_number", {"a": {"avg": {"field": "n", "missing": "abc"}}})
    add("metric_missing_string_number", {"a": {"avg": {"field": "n", "missing": "7"}, }, "b": {"max": {"field": "d", "missing": "1e3"}}})
    add("metric_es_sigma_negative", {"a": {"extended_stats": {"field": "n", "sigma": -1}}})
    add("metric_es_sigma_zero", {"a": {"extended_stats": {"field": "n", "sigma": 0}}})
    add("metric_card_negative", {"a": {"cardinality": {"field": "k", "precision_threshold": -1}}})
    add("metric_card_huge", {"a": {"cardinality": {"field": "tag", "precision_threshold": 1000000}}})
    add("metric_card_zero", {"a": {"cardinality": {"field": "tag", "precision_threshold": 0}}, "b": {"cardinality": {"field": "n", "precision_threshold": 1}}})
    add("metric_card_hint", {"a": {"cardinality": {"field": "k", "execution_hint": "direct"}}, "b": {"cardinality": {"field": "tag", "execution_hint": "save_memory_heuristic"}}})
    add("metric_card_bad_hint", {"a": {"cardinality": {"field": "k", "execution_hint": "nope"}}})
    add("metric_value_count_unmapped", {"a": {"value_count": {"field": "nope"}}, "b": {"value_count": {"field": "txt"}}})
    add("metric_value_type_bad", {"a": {"avg": {"field": "nope", "value_type": "nope"}}})
    add("metric_value_type_unmapped", {"a": {"avg": {"field": "nope", "value_type": "long", "missing": 3}}, "b": {"terms": {"field": "nope", "value_type": "date", "missing": "2020-01-01"}}})
    add("metric_format_number", {"a": {"avg": {"field": "n", "format": "0.000E0"}}, "b": {"min": {"field": "t", "format": "yyyy"}}, "c": {"sum": {"field": "b", "format": "#"}}})
    add("metric_format_bad", {"a": {"avg": {"field": "t", "format": "yyyy-qq-nonsense'"}}})
    add("metric_weighted_no_weight", {"a": {"weighted_avg": {"value": {"field": "d"}}}})
    add("metric_weighted_keyword", {"a": {"weighted_avg": {"value": {"field": "k"}, "weight": {"field": "n"}}}})
    add("metric_weighted_multi", {"a": {"weighted_avg": {"value": {"field": "n"}, "weight": {"field": "i"}}}})
    add("metric_geo_bounds_keyword", {"a": {"geo_bounds": {"field": "k"}}})
    add("metric_geo_centroid_number", {"a": {"geo_centroid": {"field": "n"}}})
    add("metric_avg_bool_date", {"a": {"avg": {"field": "b"}}, "b": {"sum": {"field": "t"}}, "c": {"stats": {"field": "b"}}})
    # Terms.
    add("terms_size_zero", {"t": {"terms": {"field": "k", "size": 0}}})
    add("terms_size_negative", {"t": {"terms": {"field": "k", "size": -1}}})
    add("terms_shard_size_zero", {"t": {"terms": {"field": "k", "shard_size": 0}}})
    add("terms_shard_size_small", {"t": {"terms": {"field": "tag", "size": 5, "shard_size": 2, "show_term_doc_count_error": True}}})
    add("terms_shard_size_minus_one", {"t": {"terms": {"field": "tag", "size": 3, "shard_size": -1}}})
    add("terms_min_doc_count_negative", {"t": {"terms": {"field": "k", "min_doc_count": -1}}})
    add("terms_shard_min_doc_count_negative", {"t": {"terms": {"field": "k", "shard_min_doc_count": -1}}})
    add("terms_shard_min_doc_count", {"t": {"terms": {"field": "tag", "shard_min_doc_count": 3, "size": 5}}})
    add("terms_order_unknown_agg", {"t": {"terms": {"field": "k", "order": {"nope": "asc"}}}})
    add("terms_order_bad_direction", {"t": {"terms": {"field": "k", "order": {"_count": "up"}}}})
    add("terms_order_term", {"t": {"terms": {"field": "k", "order": {"_term": "asc"}}}})
    add("terms_order_multi_value_no_key", {"t": {"terms": {"field": "k", "order": {"s": "asc"}}, "aggs": {"s": {"stats": {"field": "n"}}}}})
    add("terms_order_multi_value_bad_key", {"t": {"terms": {"field": "k", "order": {"s.nope": "asc"}}, "aggs": {"s": {"stats": {"field": "n"}}}}})
    add("terms_order_bracket_key", {"t": {"terms": {"field": "k", "order": {"p[50.0]": "desc"}, "size": 3}, "aggs": {"p": {"percentiles": {"field": "n"}}}}})
    add("terms_order_percentile_dot", {"t": {"terms": {"field": "k", "order": {"p.99": "desc"}, "size": 3}, "aggs": {"p": {"percentiles": {"field": "n", "percents": [99]}}}}})
    add("terms_order_bucket_agg", {"t": {"terms": {"field": "k", "order": {"i": "asc"}}, "aggs": {"i": {"terms": {"field": "tag"}}}}})
    add("terms_order_through_terms", {"t": {"terms": {"field": "k", "order": {"i>m": "asc"}}, "aggs": {"i": {"terms": {"field": "tag"}, "aggs": {"m": {"max": {"field": "n"}}}}}}})
    add("terms_order_filter_count", {"t": {"terms": {"field": "k", "order": {"f._count": "desc"}, "size": 4}, "aggs": {"f": {"filter": {"term": {"b": True}}}}}})
    add("terms_order_filter_gt_count", {"t": {"terms": {"field": "k", "order": {"f>_count": "desc"}, "size": 4}, "aggs": {"f": {"filter": {"term": {"b": True}}}}}})
    add("terms_order_filter_direct", {"t": {"terms": {"field": "k", "order": {"f": "desc"}, "size": 4}, "aggs": {"f": {"filter": {"term": {"b": True}}}}}})
    add("terms_order_deep_path", {"t": {"terms": {"field": "k", "order": [{"f>m>s.avg": "asc"}, {"_key": "desc"}], "size": 5},
                                        "aggs": {"f": {"filter": {"range": {"n": {"gte": 20}}}, "aggs": {"m": {"missing": {"field": "ip"}, "aggs": {"s": {"stats": {"field": "d"}}}}, "x": {"missing": {"field": "d"}, "aggs": {"s": {"stats": {"field": "n"}}}}}}}}})
    add("terms_order_ties", {"t": {"terms": {"field": "tag", "order": [{"mx": "desc"}, {"_count": "asc"}], "size": 8}, "aggs": {"mx": {"max": {"field": "i"}}}}})
    add("terms_order_key_count_numeric", {"t": {"terms": {"field": "i", "order": [{"_count": "desc"}, {"_key": "asc"}], "size": 6, "show_term_doc_count_error": True}}})
    add("terms_order_count_asc_error", {"t": {"terms": {"field": "tag", "order": {"_count": "asc"}, "size": 4, "show_term_doc_count_error": True}}})
    add("terms_order_sub_error", {"t": {"terms": {"field": "tag", "order": {"m": "desc"}, "size": 4, "show_term_doc_count_error": True}, "aggs": {"m": {"min": {"field": "d"}}}}})
    add("terms_order_object_array", {"t": {"terms": {"field": "k", "order": [{"_count": "desc", "_key": "asc"}]}}})
    add("terms_order_not_object", {"t": {"terms": {"field": "k", "order": "_count"}}})
    add("terms_regex_numeric", {"t": {"terms": {"field": "n", "include": "1.*"}}})
    add("terms_include_partition_missing_num", {"t": {"terms": {"field": "k", "include": {"partition": 1}}}})
    add("terms_include_partition_out_of_range", {"t": {"terms": {"field": "k", "include": {"partition": 3, "num_partitions": 3}}}})
    add("terms_include_partition_and_exclude", {"t": {"terms": {"field": "k", "include": {"partition": 0, "num_partitions": 2}, "exclude": "k0[0-3]"}}})
    add("terms_include_regex_bad", {"t": {"terms": {"field": "k", "include": "k0[0-"}}})
    add("terms_include_mixed", {"t": {"terms": {"field": "k", "include": "k0.*", "exclude": ["k01", "k02"]}}})
    add("terms_include_unknown_key", {"t": {"terms": {"field": "k", "include": {"foo": 1}}}})
    add("terms_include_numbers_double", {"t": {"terms": {"field": "d", "include": [50.0, 41.513, 7], "size": 5}}, "u": {"terms": {"field": "b", "exclude": ["true"]}}})
    add("terms_include_ip_date", {"t": {"terms": {"field": "ip", "include": ["10.0.0.3", "2001:db8::2"]}}, "u": {"terms": {"field": "ip", "include": "10\\.0\\.0\\.1.*", "size": 3}}})
    add("terms_bad_hint", {"t": {"terms": {"field": "k", "execution_hint": "nope"}}})
    add("terms_bad_collect_mode", {"t": {"terms": {"field": "k", "collect_mode": "nope"}}})
    add("terms_collect_modes", {"t": {"terms": {"field": "k", "collect_mode": "breadth_first", "size": 3}, "aggs": {"i": {"terms": {"field": "tag", "size": 2}, "aggs": {"a": {"avg": {"field": "d"}}}}}},
                                "u": {"terms": {"field": "n", "collect_mode": "depth_first", "size": 3, "execution_hint": "global_ordinals"}, "aggs": {"m": {"max": {"field": "t"}}}}})
    add("terms_breadth_first_order_sub", {"t": {"terms": {"field": "k", "collect_mode": "breadth_first", "order": {"a": "desc"}, "size": 3}, "aggs": {"a": {"avg": {"field": "d"}}}}})
    add("terms_geo", {"t": {"terms": {"field": "g"}}})
    add("terms_missing_type_mismatch", {"t": {"terms": {"field": "n", "missing": "abc"}}})
    add("terms_missing_bool_date_ip", {"b": {"terms": {"field": "b", "missing": False}}, "t": {"terms": {"field": "t", "missing": "2000-01-01", "order": {"_key": "asc"}, "size": 2}},
                                       "ip": {"terms": {"field": "ip", "missing": "1.2.3.4", "order": {"_key": "asc"}, "size": 2}}})
    add("terms_missing_ip_bad", {"ip": {"terms": {"field": "ip", "missing": "nope"}}})
    add("terms_format", {"t": {"terms": {"field": "n", "format": "000.0", "size": 3}}, "d": {"terms": {"field": "t", "format": "yyyy-MM", "size": 2}}, "k": {"terms": {"field": "k", "format": "0.0", "size": 2}}})
    add("terms_text", {"t": {"terms": {"field": "txt"}}})
    add("terms_min_doc_count_zero_numeric", {"t": {"terms": {"field": "i", "min_doc_count": 0, "size": 5, "order": {"_key": "asc"}}}, "b": {"terms": {"field": "b", "min_doc_count": 0}}},
        query={"range": {"i": {"gte": 40}}})
    add("terms_min_doc_count_zero_subs", {"t": {"terms": {"field": "k", "min_doc_count": 0, "size": 20}, "aggs": {"a": {"avg": {"field": "n"}}, "c": {"value_count": {"field": "n"}}}}},
        query={"term": {"k": "k05"}})
    add("terms_size_huge", {"t": {"terms": {"field": "tag", "size": 100000}}})
    add("terms_shard_size_less_than_size", {"t": {"terms": {"field": "tag", "size": 10, "shard_size": 3, "show_term_doc_count_error": True}}})
    add("terms_sum_other_query", {"t": {"terms": {"field": "tag", "size": 2, "show_term_doc_count_error": True}, "aggs": {"x": {"terms": {"field": "k", "size": 2, "show_term_doc_count_error": True}}}}},
        query={"range": {"n": {"lt": 40}}})
    # Multi terms / rare terms / significant.
    add("multi_terms_order_sub", {"m": {"multi_terms": {"terms": [{"field": "k"}, {"field": "b"}], "order": {"a": "desc"}, "size": 4}, "aggs": {"a": {"avg": {"field": "n"}}}}})
    add("multi_terms_missing", {"m": {"multi_terms": {"terms": [{"field": "k", "missing": "?"}, {"field": "d", "missing": 0}], "size": 3, "show_term_doc_count_error": True}}})
    add("multi_terms_size_zero", {"m": {"multi_terms": {"terms": [{"field": "k"}, {"field": "b"}], "size": 0}}})
    add("multi_terms_unmapped", {"m": {"multi_terms": {"terms": [{"field": "k"}, {"field": "nope"}]}}})
    add("rare_terms_precision", {"r": {"rare_terms": {"field": "tag", "max_doc_count": 1, "precision": 0.00001}}})
    add("rare_terms_precision_bad", {"r": {"rare_terms": {"field": "tag", "precision": 0.000001}}})
    add("sig_terms_size_zero", {"s": {"significant_terms": {"field": "k", "size": 0}}})
    add("sig_terms_two_heuristics", {"s": {"significant_terms": {"field": "k", "jlh": {}, "gnd": {}}}})
    add("sig_terms_unknown_heuristic_suggest", {"s": {"significant_terms": {"field": "k", "jlp": {}}}})
    add("sig_terms_unknown_field", {"s": {"significant_terms": {"field": "k", "nope": {}}}})
    add("sig_terms_unknown_value_field", {"s": {"significant_terms": {"field": "k", "min_doc_cont": 1}}})
    add("sig_text_unknown_heuristic_suggest", {"s": {"significant_text": {"field": "txt", "chi_squar": {}}}})
    add("sig_terms_nested_field", {"n": {"nested": {"path": "nest"}, "aggs": {"s": {"significant_terms": {"field": "nest.y", "min_doc_count": 1}}, "x": {"significant_terms": {"field": "nest.x", "min_doc_count": 1}}}}},
        query={"term": {"b": True}})
    add("sig_terms_nested_field_bg", {"n": {"nested": {"path": "nest"}, "aggs": {"s": {"significant_terms": {"field": "nest.y", "min_doc_count": 1, "background_filter": {"term": {"b": True}}}},
                                                                                  "x": {"significant_terms": {"field": "nest.x", "min_doc_count": 1, "background_filter": {"range": {"n": {"lt": 50}}}, "chi_square": {}}}}}},
        query={"term": {"b": True}})
    add("sig_terms_nested_in_terms", {"t": {"terms": {"field": "k", "size": 3}, "aggs": {"n": {"nested": {"path": "nest"}, "aggs": {"s": {"significant_terms": {"field": "nest.y", "min_doc_count": 1, "gnd": {}}}}}}}})
    add("sig_terms_mi_options", {"t": {"terms": {"field": "b"}, "aggs": {"s": {"significant_terms": {"field": "k", "mutual_information": {"include_negatives": True, "background_is_superset": False}, "background_filter": {"range": {"n": {"lt": 70}}}, "min_doc_count": 1}}}}})
    add("sig_terms_chi_percentage", {"t": {"terms": {"field": "b"}, "aggs": {"s": {"significant_terms": {"field": "tag", "chi_square": {}, "min_doc_count": 2, "size": 4}},
                                                                     "p": {"significant_terms": {"field": "k", "percentage": {}, "size": 3}}}}})
    # Histograms.
    add("histo_interval_zero", {"h": {"histogram": {"field": "n", "interval": 0}}})
    add("histo_interval_negative", {"h": {"histogram": {"field": "n", "interval": -5}}})
    add("histo_no_interval", {"h": {"histogram": {"field": "n"}}})
    add("histo_min_doc_count_negative", {"h": {"histogram": {"field": "n", "interval": 5, "min_doc_count": -1}}})
    add("histo_bounds_inverted", {"h": {"histogram": {"field": "n", "interval": 5, "extended_bounds": {"min": 50, "max": 10}}}})
    add("histo_hard_bounds_inverted", {"h": {"histogram": {"field": "n", "interval": 5, "hard_bounds": {"min": 50, "max": 10}}}})
    add("histo_bounds_outside_hard", {"h": {"histogram": {"field": "n", "interval": 5, "min_doc_count": 0, "extended_bounds": {"min": 0, "max": 200}, "hard_bounds": {"min": 10, "max": 60}}}})
    add("histo_keyword", {"h": {"histogram": {"field": "k", "interval": 5}}})
    add("histo_geo", {"h": {"histogram": {"field": "g", "interval": 5}}})
    add("histo_date_field", {"h": {"histogram": {"field": "t", "interval": 86400000000}}, "b": {"histogram": {"field": "b", "interval": 1}}})
    add("histo_unmapped_bounds", {"h": {"histogram": {"field": "nope", "interval": 5, "min_doc_count": 0, "extended_bounds": {"min": 0, "max": 20}}},
                                  "m": {"histogram": {"field": "nope", "interval": 5, "missing": 7}}})
    add("histo_bad_order", {"h": {"histogram": {"field": "n", "interval": 5, "order": {"_count": "sideways"}}}})
    add("histo_order_unknown", {"h": {"histogram": {"field": "n", "interval": 5, "order": {"nope": "asc"}}}})
    add("histo_tiny_interval", {"h": {"histogram": {"field": "f", "interval": 0.3, "offset": 0.1, "min_doc_count": 1}}})
    add("histo_offset_large", {"h": {"histogram": {"field": "n", "interval": 10, "offset": 107}}, "o": {"histogram": {"field": "n", "interval": 10, "offset": -3.5}}})
    add("histo_too_many", {"h": {"histogram": {"field": "n", "interval": 0.001, "min_doc_count": 0}}})
    add("histo_too_many_bounds", {"h": {"histogram": {"field": "n", "interval": 1, "min_doc_count": 0, "extended_bounds": {"min": 0, "max": 70000}}}})
    add("histo_just_under_limit", {"h": {"histogram": {"field": "n", "interval": 1, "min_doc_count": 0, "extended_bounds": {"min": 0, "max": 60000}}, "aggs": {"bc": {"max_bucket": {"buckets_path": "_count"}}}}})
    add("histo_too_many_nested", {"t": {"terms": {"field": "tag", "size": 300}, "aggs": {"h": {"histogram": {"field": "n", "interval": 1, "min_doc_count": 0, "extended_bounds": {"min": 0, "max": 300}}}}}})
    add("histo_extended_bounds_strings", {"h": {"histogram": {"field": "n", "interval": 10, "min_doc_count": 0, "extended_bounds": {"min": "-20", "max": "120"}}}})
    add("histo_keyed_format", {"h": {"histogram": {"field": "d", "interval": 15, "keyed": True, "format": "#.#"}}})
    # Date histograms.
    add("dh_no_interval", {"h": {"date_histogram": {"field": "t"}}})
    add("dh_both_intervals", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "fixed_interval": "30d"}}})
    add("dh_legacy_interval", {"h": {"date_histogram": {"field": "t", "interval": "month"}}})
    add("dh_bad_calendar", {"h": {"date_histogram": {"field": "t", "calendar_interval": "2d"}}})
    add("dh_bad_calendar_word", {"h": {"date_histogram": {"field": "t", "calendar_interval": "fortnight"}}})
    add("dh_fixed_month", {"h": {"date_histogram": {"field": "t", "fixed_interval": "1M"}}})
    add("dh_fixed_bad", {"h": {"date_histogram": {"field": "t", "fixed_interval": "abc"}}})
    add("dh_fixed_zero", {"h": {"date_histogram": {"field": "t", "fixed_interval": "0d"}}})
    add("dh_bad_tz", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "time_zone": "Mars/Base"}}})
    add("dh_bad_offset", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "offset": "abc"}}})
    add("dh_bad_bounds", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "extended_bounds": {"min": "not a date", "max": "2021-01-01"}}}})
    add("dh_bounds_inverted", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "extended_bounds": {"min": "2021-01-01", "max": "2020-01-01"}}}})
    add("dh_keyword", {"h": {"date_histogram": {"field": "k", "calendar_interval": "month"}}})
    add("dh_long_field", {"h": {"date_histogram": {"field": "n", "fixed_interval": "10ms"}}})
    add("dh_calendar_units", {"m": {"date_histogram": {"field": "t", "calendar_interval": "1q", "time_zone": "Asia/Kathmandu"}},
                              "w": {"date_histogram": {"field": "t", "calendar_interval": "1w", "time_zone": "-03:00", "min_doc_count": 5}},
                              "y": {"date_histogram": {"field": "t", "calendar_interval": "year", "format": "epoch_second"}}})
    add("dh_fixed_units", {"a": {"date_histogram": {"field": "t", "fixed_interval": "90m", "min_doc_count": 2}},
                           "b": {"date_histogram": {"field": "t", "fixed_interval": "1000s", "min_doc_count": 2, "time_zone": "Australia/Lord_Howe"}},
                           "c": {"date_histogram": {"field": "t", "fixed_interval": "7d", "offset": "-1h", "format": "date_time"}}},
        query={"range": {"t": {"gte": 1600000000000, "lt": 1601000000000}}})
    add("dh_tz_dst_hours", {"h": {"date_histogram": {"field": "t", "calendar_interval": "hour", "time_zone": "Europe/London", "min_doc_count": 0}}},
        query={"range": {"t": {"gte": 1603584000000, "lt": 1603670400000}}})
    add("dh_tz_fixed_dst", {"h": {"date_histogram": {"field": "t", "fixed_interval": "3h", "time_zone": "America/Chicago", "min_doc_count": 0}}},
        query={"range": {"t": {"gte": 1604210400000, "lt": 1604307600000}}})
    add("dh_extended_bounds_tz", {"h": {"date_histogram": {"field": "t", "calendar_interval": "day", "time_zone": "Pacific/Auckland", "min_doc_count": 0,
                                                          "extended_bounds": {"min": "2020-09-20", "max": "2020-10-05"}, "format": "yyyy-MM-dd"}}},
        query={"range": {"t": {"gte": 1600560000000, "lt": 1601856000000}}})
    add("dh_offset_calendar", {"h": {"date_histogram": {"field": "t", "calendar_interval": "day", "offset": "+6h", "time_zone": "Europe/Paris", "min_doc_count": 0}}},
        query={"range": {"t": {"gte": 1585180800000, "lt": 1585785600000}}})
    add("dh_missing_bad", {"h": {"date_histogram": {"field": "t", "calendar_interval": "year", "missing": "nope"}}})
    add("dh_too_many", {"h": {"date_histogram": {"field": "t", "fixed_interval": "1s", "min_doc_count": 0}}})
    add("dh_keyed_order", {"h": {"date_histogram": {"field": "t", "calendar_interval": "quarter", "keyed": True, "order": [{"_count": "asc"}, {"_key": "desc"}]}}})
    add("dh_hard_bounds_with_offset", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "offset": "15d", "min_doc_count": 0,
                                                               "hard_bounds": {"min": "2020-02-10", "max": "2020-05-20"}}}})
    add("dh_bounds_epoch_numbers", {"h": {"date_histogram": {"field": "t", "calendar_interval": "year", "min_doc_count": 0,
                                                            "extended_bounds": {"min": 1500000000000, "max": 1700000000000}}}})
    add("auto_dh_minimum_interval", {"a": {"auto_date_histogram": {"field": "t", "buckets": 4, "minimum_interval": "month"}},
                                     "b": {"auto_date_histogram": {"field": "t", "buckets": 30, "time_zone": "Europe/Paris", "format": "yyyy-MM-dd"}}})
    # Ranges.
    add("range_no_ranges", {"r": {"range": {"field": "n"}}})
    add("range_empty_ranges", {"r": {"range": {"field": "n", "ranges": []}}})
    add("range_keyword", {"r": {"range": {"field": "k", "ranges": [{"to": 5}]}}})
    add("range_string_bounds", {"r": {"range": {"field": "n", "ranges": [{"from": "10", "to": "20"}, {"from": "abc"}]}}})
    add("range_inverted", {"r": {"range": {"field": "n", "ranges": [{"from": 50, "to": 10}, {"from": 10, "to": 50}]}}})
    add("range_unknown_key", {"r": {"range": {"field": "n", "ranges": [{"fromm": 50}]}}})
    add("range_format_unmapped_missing", {"r": {"range": {"field": "nope", "missing": 15, "format": "0.0", "ranges": [{"to": 10}, {"from": 10, "to": 20}]}}})
    add("range_date_field", {"r": {"range": {"field": "t", "ranges": [{"to": 1600000000000}, {"from": 1600000000000}]}}})
    add("date_range_bad_date", {"r": {"date_range": {"field": "t", "ranges": [{"to": "not a date"}]}}})
    add("date_range_numeric_field", {"r": {"date_range": {"field": "n", "ranges": [{"to": 50}, {"from": "50"}]}}})
    add("date_range_math", {"r": {"date_range": {"field": "t", "format": "yyyy-MM-dd", "ranges": [{"from": "2020-01-01||+1M/M", "to": "2020-06-01||-1d"}, {"from": "2020-06-01||/y"}]}}})
    add("date_range_epoch", {"r": {"date_range": {"field": "t", "format": "epoch_millis", "ranges": [{"to": 1600000000000}, {"from": "1600000000000"}]}}})
    add("ip_range_bad_mask", {"r": {"ip_range": {"field": "ip", "ranges": [{"mask": "10.0.0.0/33"}]}}})
    add("ip_range_mask_and_from", {"r": {"ip_range": {"field": "ip", "ranges": [{"mask": "10.0.0.0/8", "from": "10.0.0.1"}]}}})
    add("ip_range_bad_ip", {"r": {"ip_range": {"field": "ip", "ranges": [{"from": "300.0.0.1"}]}}})
    add("ip_range_numeric_field", {"r": {"ip_range": {"field": "n", "ranges": [{"to": "10.0.0.1"}]}}})
    add("ip_prefix_too_long", {"p": {"ip_prefix": {"field": "ip", "prefix_length": 33}}})
    add("ip_prefix_v6_too_long", {"p": {"ip_prefix": {"field": "ip", "prefix_length": 129, "is_ipv6": True}}})
    add("ip_prefix_negative", {"p": {"ip_prefix": {"field": "ip", "prefix_length": -1}}})
    add("ip_prefix_keyword", {"p": {"ip_prefix": {"field": "k", "prefix_length": 8}}})
    add("ip_prefix_no_length", {"p": {"ip_prefix": {"field": "ip"}}})
    add("ip_prefix_min_doc_count", {"p": {"ip_prefix": {"field": "ip", "prefix_length": 30, "min_doc_count": 3}}, "z": {"ip_prefix": {"field": "ip", "prefix_length": 0}}})
    add("geo_distance_no_origin", {"g": {"geo_distance": {"field": "g", "ranges": [{"to": 10}]}}})
    add("geo_distance_bad_unit", {"g": {"geo_distance": {"field": "g", "origin": "0,0", "unit": "parsec", "ranges": [{"to": 10}]}}})
    add("geo_distance_keyword", {"g": {"geo_distance": {"field": "k", "origin": "0,0", "ranges": [{"to": 10}]}}})
    add("geo_distance_origin_forms", {"a": {"geo_distance": {"field": "g", "origin": {"lat": 10, "lon": 20}, "unit": "mi", "ranges": [{"to": 1000}, {"from": 1000}]}},
                                      "b": {"geo_distance": {"field": "g", "origin": "s1z0", "ranges": [{"to": 5000000}, {"from": 5000000}]}},
                                      "c": {"geo_distance": {"field": "g", "origin": "POINT (20 10)", "unit": "nmi", "ranges": [{"from": 100, "to": 3000}]}}})
    # Filters and friends.
    add("filters_empty", {"f": {"filters": {"filters": {}}}})
    add("filters_empty_list", {"f": {"filters": {"filters": []}}})
    add("filters_not_object", {"f": {"filters": {"filters": 5}}})
    add("filters_no_filters", {"f": {"filters": {"other_bucket": True}}})
    add("filters_other_key_only", {"f": {"filters": {"filters": {"a": {"term": {"k": "k00"}}}, "other_bucket_key": "o"}},
                                   "g": {"filters": {"filters": {"a": {"term": {"k": "k00"}}}, "other_bucket": False, "other_bucket_key": "o"}}})
    add("filters_keyed_false_other", {"f": {"filters": {"filters": {"b": {"term": {"k": "k00"}}, "a": {"term": {"k": "k01"}}}, "keyed": False, "other_bucket": True}}})
    add("filters_unknown_field", {"f": {"filters": {"filters": {"a": {"match_all": {}}}, "foo": 1}}})
    add("adjacency_separator", {"a": {"adjacency_matrix": {"separator": "+", "filters": {"x": {"term": {"k": "k00"}}, "y": {"term": {"b": True}}}}}})
    add("adjacency_empty", {"a": {"adjacency_matrix": {"filters": {}}}})
    add("adjacency_no_filters", {"a": {"adjacency_matrix": {}}})
    add("adjacency_too_many", {"a": {"adjacency_matrix": {"filters": {"f%03d" % i: {"term": {"i": i - 50}} for i in range(101)}}}})
    add("missing_no_field", {"m": {"missing": {}}})
    add("nested_not_nested", {"n": {"nested": {"path": "k"}}})
    add("nested_unknown_path", {"n": {"nested": {"path": "nope"}}})
    add("nested_no_path", {"n": {"nested": {}}})
    add("reverse_nested_top", {"r": {"reverse_nested": {}}})
    add("reverse_nested_bad_path", {"n": {"nested": {"path": "nest"}, "aggs": {"r": {"reverse_nested": {"path": "k"}}}}})
    add("sampler_shard_size_zero", {"s": {"sampler": {"shard_size": 0}, "aggs": {"k": {"terms": {"field": "k"}}}}})
    add("diversified_no_field", {"s": {"diversified_sampler": {"shard_size": 10}}})
    add("diversified_max_per_value", {"s": {"diversified_sampler": {"field": "n", "shard_size": 30, "max_docs_per_value": 2}, "aggs": {"a": {"avg": {"field": "n"}}, "c": {"cardinality": {"field": "n"}}}}},
        query={"range": {"d": {"gte": 40}}})
    add("geohash_precision_bad", {"g": {"geohash_grid": {"field": "g", "precision": 13}}})
    add("geotile_precision_bad", {"g": {"geotile_grid": {"field": "g", "precision": 30}}})
    add("geotile_bounds", {"g": {"geotile_grid": {"field": "g", "precision": 4, "bounds": {"top_left": "50, -40", "bottom_right": "-10, 60"}, "size": 5, "shard_size": 7}}})
    add("geohash_size_zero", {"g": {"geohash_grid": {"field": "g", "size": 0}}})
    # Pipelines.
    add("pipe_unknown_path", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {"buckets_path": "h>nope"}}})
    add("pipe_unknown_top", {"a": {"avg_bucket": {"buckets_path": "nope>x"}}})
    add("pipe_path_to_multi_bucket", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"t": {"terms": {"field": "k"}}}}, "a": {"avg_bucket": {"buckets_path": "h>t"}}})
    add("pipe_path_to_multi_value", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"stats": {"field": "d"}}}}, "a": {"max_bucket": {"buckets_path": "h>s"}}})
    add("pipe_path_multi_value_key", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"stats": {"field": "d"}}, "p": {"percentiles": {"field": "d"}}}},
                                      "a": {"max_bucket": {"buckets_path": "h>s.avg"}}, "b": {"min_bucket": {"buckets_path": "h>p[99.0]"}}, "c": {"sum_bucket": {"buckets_path": "h>p.50"}}})
    add("pipe_path_bad_value_key", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"stats": {"field": "d"}}}}, "a": {"max_bucket": {"buckets_path": "h>s.nope"}}})
    add("pipe_sibling_of_metric", {"m": {"max": {"field": "n"}}, "a": {"avg_bucket": {"buckets_path": "m"}}})
    add("pipe_sibling_no_gt", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {"buckets_path": "h"}}})
    add("pipe_sibling_count", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {"buckets_path": "h._count"}}, "b": {"max_bucket": {"buckets_path": "h>_count"}},
                               "c": {"sum_bucket": {"buckets_path": "h._bucket_count"}}})
    add("pipe_bucket_count", {"t": {"terms": {"field": "k", "size": 20}, "aggs": {"h": {"histogram": {"field": "n", "interval": 30}},
                                                                           "bs": {"bucket_selector": {"buckets_path": {"c": "h._bucket_count"}, "script": "params.c > 3"}}}}})
    add("pipe_no_buckets_path", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {}}})
    add("pipe_buckets_path_array", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {"buckets_path": ["h>_count"]}}})
    add("pipe_buckets_path_two", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {"buckets_path": ["h>_count", "h>_count"]}}})
    add("pipe_buckets_path_object", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {"buckets_path": {"x": "h>_count"}}}})
    add("pipe_bad_gap_policy", {"h": {"histogram": {"field": "n", "interval": 10}}, "a": {"avg_bucket": {"buckets_path": "h>_count", "gap_policy": "nope"}}})
    add("pipe_gap_policies", {"h": {"histogram": {"field": "n", "interval": 7, "min_doc_count": 0, "extended_bounds": {"min": -30, "max": 130}}, "aggs": {"a": {"avg": {"field": "d"}},
        "d1": {"derivative": {"buckets_path": "a"}}, "d2": {"derivative": {"buckets_path": "a", "gap_policy": "insert_zeros"}}, "d3": {"derivative": {"buckets_path": "a", "gap_policy": "keep_values"}},
        "s1": {"serial_diff": {"buckets_path": "a", "lag": 3, "gap_policy": "insert_zeros"}}, "m1": {"moving_fn": {"buckets_path": "a", "window": 3, "script": "MovingFunctions.max(values)", "gap_policy": "insert_zeros"}},
        "m2": {"moving_fn": {"buckets_path": "a", "window": 5, "shift": 2, "script": "MovingFunctions.stdDev(values, MovingFunctions.unweightedAvg(values))"}}}},
        "ab": {"avg_bucket": {"buckets_path": "h>a", "gap_policy": "insert_zeros"}}, "sb": {"stats_bucket": {"buckets_path": "h>a", "gap_policy": "keep_values"}}})
    add("pipe_derivative_parent", {"t": {"terms": {"field": "k"}, "aggs": {"c": {"derivative": {"buckets_path": "_count"}}}}})
    add("pipe_derivative_top", {"d": {"derivative": {"buckets_path": "_count"}}})
    add("pipe_derivative_unit_histogram", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"d": {"derivative": {"buckets_path": "_count", "unit": "1d"}}}}})
    add("pipe_derivative_bad_unit", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"d": {"derivative": {"buckets_path": "_count", "unit": "fortnight"}}}}})
    add("pipe_derivative_min_doc_count", {"h": {"histogram": {"field": "n", "interval": 10, "min_doc_count": 5}, "aggs": {"d": {"derivative": {"buckets_path": "_count"}}}}})
    add("pipe_derivative_units", {"h": {"date_histogram": {"field": "t", "calendar_interval": "quarter"}, "aggs": {"s": {"sum": {"field": "n"}},
        "a": {"derivative": {"buckets_path": "s", "unit": "month"}}, "b": {"derivative": {"buckets_path": "s", "unit": "12h", "format": "0.00"}}, "c": {"derivative": {"buckets_path": "_count", "unit": "week"}}}}})
    add("pipe_cumsum_gap", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"c": {"cumulative_sum": {"buckets_path": "_count", "gap_policy": "skip"}}}}})
    add("pipe_cumsum_parent", {"t": {"terms": {"field": "k"}, "aggs": {"c": {"cumulative_sum": {"buckets_path": "_count"}}}}})
    add("pipe_cumsum_format", {"h": {"histogram": {"field": "n", "interval": 20}, "aggs": {"m": {"max": {"field": "d"}}, "c": {"cumulative_sum": {"buckets_path": "m", "format": "0.0"}}}}})
    add("pipe_cumcard_not_card", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"m": {"max": {"field": "d"}}, "c": {"cumulative_cardinality": {"buckets_path": "m"}}}}})
    add("pipe_moving_fn_window_zero", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"m": {"moving_fn": {"buckets_path": "_count", "window": 0, "script": "MovingFunctions.sum(values)"}}}}})
    add("pipe_moving_fn_no_window", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"m": {"moving_fn": {"buckets_path": "_count", "script": "MovingFunctions.sum(values)"}}}}})
    add("pipe_moving_fn_no_script", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"m": {"moving_fn": {"buckets_path": "_count", "window": 3}}}}})
    add("pipe_moving_fn_parent", {"t": {"terms": {"field": "k"}, "aggs": {"m": {"moving_fn": {"buckets_path": "_count", "window": 3, "script": "MovingFunctions.sum(values)"}}}}})
    add("pipe_moving_fn_models", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"s": {"sum": {"field": "d"}},
        "e": {"moving_fn": {"buckets_path": "s", "window": 6, "script": "MovingFunctions.ewma(values, 0.3)"}},
        "h1": {"moving_fn": {"buckets_path": "s", "window": 6, "script": "MovingFunctions.holt(values, 0.3, 0.1)"}},
        "mn": {"moving_fn": {"buckets_path": "s", "window": 4, "shift": 4, "script": "MovingFunctions.min(values)"}},
        "sm": {"moving_fn": {"buckets_path": "_count", "window": 3, "shift": -1, "script": "MovingFunctions.sum(values)"}}}}})
    add("pipe_moving_fn_holt_winters", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"sum": {"field": "d"}},
        "hw": {"moving_fn": {"buckets_path": "s", "window": 8, "script": "MovingFunctions.holtWinters(values, 0.3, 0.1, 0.1, 2, false)"}}}}})
    add("pipe_moving_fn_holt_winters_mult", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"sum": {"field": "d"}},
        "hm": {"moving_fn": {"buckets_path": "s", "window": 6, "shift": 1, "script": "MovingFunctions.holtWinters(values, 0.5, 0.2, 0.3, 1, true)"}}}}})
    add("pipe_moving_fn_bad_window_order", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"m": {"moving_fn": {"window": -1, "script": "MovingFunctions.sum(values)", "buckets_path": "_count"}}}}})
    add("pipe_normalize_format_last", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"z": {"normalize": {"method": "nope", "buckets_path": "_count", "format": "0"}}}}})
    add("pipe_normalize_missing", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"z": {"normalize": {}}}}})
    add("pipe_bucket_sort_queue_order", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"z": {"bucket_sort": {"from": -1, "size": 0}}}}})
    add("pipe_bucket_sort_after_sort", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"z": {"bucket_sort": {"from": -1, "sort": [], "size": 0}}}}})
    add("histo_bounds_reversed_fields", {"h": {"histogram": {"field": "n", "interval": 10, "extended_bounds": {"max": 10, "min": 50}}}})
    add("histo_bounds_infinite", {"h": {"histogram": {"field": "n", "interval": 10, "hard_bounds": {"min": "Infinity", "max": 5}}}})
    add("histo_bounds_nan_only", {"h": {"histogram": {"field": "n", "interval": 10, "hard_bounds": {"max": "NaN"}}}})
    add("terms_order_filter_doc_count", {"t": {"terms": {"field": "tag", "order": {"f.doc_count": "desc"}, "size": 3}, "aggs": {"f": {"filter": {"term": {"b": True}}}}}})
    add("terms_order_missing_key", {"t": {"terms": {"field": "tag", "order": {"m._count": "desc"}, "size": 3}, "aggs": {"m": {"missing": {"field": "d"}}}}})
    add("terms_order_metric_through", {"t": {"terms": {"field": "tag", "order": {"m>x": "desc"}, "size": 3}, "aggs": {"m": {"max": {"field": "n"}}}}})
    add("terms_order_metric_bad_key", {"t": {"terms": {"field": "tag", "order": {"m.foo": "desc"}, "size": 3}, "aggs": {"m": {"max": {"field": "n"}}}}})
    add("adjacency_array", {"a": {"adjacency_matrix": {"filters": []}}})
    add("geo_distance_wkt_z", {"g": {"geo_distance": {"field": "g", "origin": "POINT (20 10 5)", "ranges": [{"to": 1}]}}})
    add("geo_distance_array_z", {"g": {"geo_distance": {"field": "g", "origin": [20, 10, 5], "ranges": [{"to": 1}]}}})
    add("geo_distance_bad_geohash", {"g": {"geo_distance": {"field": "g", "origin": "nonsense", "ranges": [{"to": 1}]}}})
    add("metric_bool_formats", {"a": {"avg": {"field": "b"}}, "s": {"sum": {"field": "b"}}, "mi": {"min": {"field": "b"}}, "ma": {"max": {"field": "b"}}, "st": {"stats": {"field": "b"}},
                                "es": {"extended_stats": {"field": "b"}}, "p": {"percentiles": {"field": "b", "percents": [50]}}, "pr": {"percentile_ranks": {"field": "b", "values": [1]}},
                                "w": {"weighted_avg": {"value": {"field": "b"}, "weight": {"field": "n"}}}, "c": {"cardinality": {"field": "b"}}})
    add("metric_bool_buckets", {"t": {"terms": {"field": "b"}, "aggs": {"m": {"max": {"field": "b"}}}}, "h": {"histogram": {"field": "b", "interval": 1}},
                                "r": {"range": {"field": "b", "ranges": [{"to": 1}, {"from": 1}]}}, "mb": {"max_bucket": {"buckets_path": "t>m"}}, "sb": {"stats_bucket": {"buckets_path": "t>m"}}})
    add("metric_weighted_no_value", {"w": {"weighted_avg": {"weight": {"field": "n"}}}})
    add("sampler_no_subs_zero", {"s": {"sampler": {"shard_size": 0}}})
    add("sampler_negative", {"s": {"sampler": {"shard_size": -1}, "aggs": {"k": {"terms": {"field": "k"}}}}})
    add("ip_prefix_v6_no_length", {"p": {"ip_prefix": {"field": "ip", "is_ipv6": True}}})
    add("pipe_serial_diff_lag_zero", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"serial_diff": {"buckets_path": "_count", "lag": 0}}}}})
    add("pipe_serial_diff_lag_negative", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"serial_diff": {"buckets_path": "_count", "lag": -2}}}}})
    add("pipe_normalize_bad_method", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"z": {"normalize": {"buckets_path": "_count", "method": "nope"}}}}})
    add("pipe_normalize_no_method", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"z": {"normalize": {"buckets_path": "_count"}}}}})
    add("pipe_normalize_methods", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"m": {"min": {"field": "d"}},
        "a": {"normalize": {"buckets_path": "m", "method": "rescale_0_1"}}, "b": {"normalize": {"buckets_path": "m", "method": "rescale_0_100", "format": "0.0"}},
        "c": {"normalize": {"buckets_path": "_count", "method": "mean"}}, "d": {"normalize": {"buckets_path": "m", "method": "softmax"}}}}})
    add("pipe_percentiles_bucket_bad", {"h": {"histogram": {"field": "n", "interval": 10}}, "p": {"percentiles_bucket": {"buckets_path": "h>_count", "percents": [101]}}})
    add("pipe_percentiles_bucket_path", {"h": {"histogram": {"field": "n", "interval": 10}}, "p": {"percentiles_bucket": {"buckets_path": "h>_count", "percents": [25, 50, 75]}},
                                         "t": {"terms": {"field": "k", "order": {"_key": "asc"}}, "aggs": {"m": {"max_bucket": {"buckets_path": "p2.50"}}}}})
    add("pipe_es_bucket_sigma_negative", {"h": {"histogram": {"field": "n", "interval": 10}}, "e": {"extended_stats_bucket": {"buckets_path": "h>_count", "sigma": -1}}})
    add("pipe_bucket_sort_parent_metric", {"m": {"max": {"field": "n"}}, "s": {"bucket_sort": {"size": 3}}})
    add("pipe_bucket_sort_from_negative", {"t": {"terms": {"field": "k"}, "aggs": {"s": {"bucket_sort": {"from": -1}}}}})
    add("pipe_bucket_sort_size_zero", {"t": {"terms": {"field": "k"}, "aggs": {"s": {"bucket_sort": {"size": 0}}}}})
    add("pipe_bucket_sort_unknown", {"t": {"terms": {"field": "k"}, "aggs": {"s": {"bucket_sort": {"sort": [{"nope": "desc"}]}}}}})
    add("pipe_bucket_sort_only_trim", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"bucket_sort": {"from": 2, "size": 3}}}}})
    add("pipe_bucket_sort_keys", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month"}, "aggs": {"a": {"avg": {"field": "n"}},
        "s": {"bucket_sort": {"sort": [{"_count": {"order": "asc"}}, {"_key": "desc"}], "size": 5, "gap_policy": "insert_zeros"}}}}})
    add("pipe_bucket_sort_missing_key_insert_zeros", {"h": {"date_histogram": {"field": "t", "calendar_interval": "quarter"}, "aggs": {"i": {"terms": {"field": "k"}},
        "s": {"bucket_sort": {"gap_policy": "insert_zeros", "sort": [{"i['nonexistent']._count": "desc"}]}}}}})
    add("pipe_bucket_sort_missing_key_skip", {"h": {"date_histogram": {"field": "t", "calendar_interval": "quarter"}, "aggs": {"i": {"terms": {"field": "k"}},
        "s": {"bucket_sort": {"sort": [{"i['k01']._count": "desc"}]}}}}})
    add("pipe_parent_under_single_bucket", {"f": {"filter": {"term": {"b": True}}, "aggs": {"b": {"sum": {"field": "n"}}, "d": {"bucket_sort": {"sort": [{"b": "asc"}]}}}}})
    add("pipe_bucket_script_under_single_bucket", {"f": {"filter": {"term": {"b": True}}, "aggs": {"b": {"sum": {"field": "n"}}, "e": {"normalize": {"buckets_path": "b", "method": "mean"}}}}})
    add("pipe_bucket_sort_multi", {"t": {"terms": {"field": "tag", "size": 50}, "aggs": {"st": {"stats": {"field": "d"}},
        "s": {"bucket_sort": {"sort": ["st.max", {"st.min": "desc"}, "_key"], "size": 6}}}}})
    add("pipe_bucket_script_no_script", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"bucket_script": {"buckets_path": {"c": "_count"}}}}}})
    add("pipe_bucket_selector_no_script", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"bucket_selector": {"buckets_path": {"c": "_count"}}}}}})
    add("pipe_two_pipelines_chain", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"s": {"sum": {"field": "d"}}, "d": {"derivative": {"buckets_path": "s"}}, "c": {"cumulative_sum": {"buckets_path": "d"}}}},
                                     "m": {"max_bucket": {"buckets_path": "h>c"}}, "mm": {"stats_bucket": {"buckets_path": "h>d"}}})
    add("pipe_sibling_of_sibling", {"t": {"terms": {"field": "k"}, "aggs": {"h": {"histogram": {"field": "n", "interval": 25}}, "m": {"max_bucket": {"buckets_path": "h>_count"}}}},
                                    "mm": {"avg_bucket": {"buckets_path": "t>m"}}, "mk": {"max_bucket": {"buckets_path": "t>m.value"}}})
    add("pipe_max_bucket_keys", {"t": {"terms": {"field": "k", "size": 20}}, "m": {"max_bucket": {"buckets_path": "t>_count"}}, "n": {"min_bucket": {"buckets_path": "t._count"}}})
    add("pipe_path_through_filter", {"t": {"terms": {"field": "k", "size": 20}, "aggs": {"f": {"filter": {"term": {"b": True}}, "aggs": {"a": {"avg": {"field": "d"}}}}}},
                                     "m": {"max_bucket": {"buckets_path": "t>f>a"}}, "c": {"avg_bucket": {"buckets_path": "t>f._count"}}})
    add("pipe_path_through_multi_bucket", {"t": {"terms": {"field": "k", "size": 20}, "aggs": {"h": {"terms": {"field": "b"}, "aggs": {"a": {"avg": {"field": "d"}}}}}},
                                           "m": {"max_bucket": {"buckets_path": "t>h>a"}}})
    add("pipe_format", {"h": {"histogram": {"field": "n", "interval": 10}, "aggs": {"a": {"avg": {"field": "d"}}}},
                        "a": {"avg_bucket": {"buckets_path": "h>a", "format": "0.00"}}, "s": {"stats_bucket": {"buckets_path": "h>a", "format": "0.0"}},
                        "e": {"extended_stats_bucket": {"buckets_path": "h>a", "format": "0.0"}}, "p": {"percentiles_bucket": {"buckets_path": "h>a", "format": "0.0", "percents": [50]}}})
    add("pipe_empty_parent", {"h": {"histogram": {"field": "nope", "interval": 10}, "aggs": {"a": {"avg": {"field": "d"}}}},
                              "a": {"avg_bucket": {"buckets_path": "h>a"}}, "m": {"max_bucket": {"buckets_path": "h>a"}}, "s": {"stats_bucket": {"buckets_path": "h>a"}},
                              "e": {"extended_stats_bucket": {"buckets_path": "h>a"}}, "p": {"percentiles_bucket": {"buckets_path": "h>a"}}, "su": {"sum_bucket": {"buckets_path": "h>a"}}})
    add("pipe_under_metric_parent", {"t": {"terms": {"field": "k"}, "aggs": {"m": {"max": {"field": "n"}}}}, "x": {"avg_bucket": {"buckets_path": "t>m>z"}}})
    # Composite edge cases.
    add("composite_size_one_pages", {"c": {"composite": {"size": 1, "sources": [{"k": {"terms": {"field": "k"}}}, {"b": {"terms": {"field": "b", "order": "desc"}}}]}}})
    add("composite_histo_missing_bucket", {"c": {"composite": {"size": 6, "sources": [{"h": {"histogram": {"field": "d", "interval": 20, "missing_bucket": True, "missing_order": "last", "order": "desc"}}}],
                                                               "after": {"h": 60.0}}, "aggs": {"m": {"max": {"field": "n"}}}}})
    add("composite_date_tz_offset", {"c": {"composite": {"size": 5, "sources": [{"d": {"date_histogram": {"field": "t", "calendar_interval": "month", "time_zone": "America/New_York", "offset": "+1d", "format": "yyyy-MM-dd"}}}],
                                                         "after": {"d": "2020-03-02"}}}})
    add("composite_after_wrong_type", {"c": {"composite": {"sources": [{"n": {"terms": {"field": "n"}}}], "after": {"n": "abc"}}}})
    add("composite_after_extra_key", {"c": {"composite": {"sources": [{"n": {"terms": {"field": "n"}}}], "after": {"n": 1, "x": 2}}}})
    add("metric_stats_date_format_overflow", {"s": {"stats": {"field": "t", "format": "date_hour_minute_second_millis"}}})
    add("metric_extended_stats_date_format_overflow", {"s": {"extended_stats": {"field": "t", "format": "basic_date"}}})
    add("metric_stats_date_iso_wide_year", {"s": {"stats": {"field": "t"}}, "e": {"stats": {"field": "t", "format": "yyyy-MM-dd"}}})
    add("metric_stats_fixed_year_formats", {f.replace("_", "")[:20]: {"stats": {"field": "t", "format": f}} for f in [
        "ordinal_date_time", "year_month_day", "strict_date_time", "strict_year", "strict_ordinal_date", "date_hour_minute_second", "year"]})
    add("metric_named_date_formats", {f: {"min": {"field": "t", "format": f}} for f in [
        "basic_date", "basic_date_time", "basic_date_time_no_millis", "basic_ordinal_date", "basic_time", "basic_t_time_no_millis", "date", "date_hour",
        "date_hour_minute", "date_hour_minute_second", "date_hour_minute_second_fraction", "date_hour_minute_second_millis", "date_time", "date_time_no_millis",
        "hour", "hour_minute", "hour_minute_second", "hour_minute_second_millis", "ordinal_date", "ordinal_date_time", "time", "time_no_millis",
        "t_time", "year", "year_month", "year_month_day", "strict_date", "strict_year_month", "strict_hour_minute", "epoch_second", "epoch_millis"]})
    add("date_range_epoch_second_numbers", {"r": {"date_range": {"field": "t", "format": "epoch_second", "ranges": [{"to": 1590000000}, {"from": 1590000000, "to": "1600000000"}, {"from": 1600000000.7}]}}})
    add("date_range_numbers_default_format", {"r": {"date_range": {"field": "t", "ranges": [{"to": 1590000000000}, {"from": 1590000000000.9}]}}})
    add("composite_size_over_limit", {"c": {"composite": {"size": 65537, "sources": [{"k": {"terms": {"field": "k"}}}]}}})
    add("composite_size_at_limit", {"c": {"composite": {"size": 65536, "sources": [{"k": {"terms": {"field": "k"}}}]}}})
    add("composite_lossy_format", {"c": {"composite": {"sources": [{"d": {"date_histogram": {"field": "t", "calendar_interval": "1d", "format": "yyyy-MM"}}}]}}})
    add("composite_lossy_format_tz", {"c": {"composite": {"sources": [{"d": {"date_histogram": {"field": "t", "calendar_interval": "1d", "format": "yyyy-MM", "time_zone": "Europe/Paris"}}}]}}})
    add("composite_format_ok", {"c": {"composite": {"size": 3, "sources": [{"d": {"date_histogram": {"field": "t", "calendar_interval": "1d", "format": "yyyy-MM-dd", "time_zone": "+01:00"}}}]}}})
    add("pipe_single_percent", {"t": {"terms": {"field": "k"}, "aggs": {"p": {"percentiles": {"field": "n", "percents": [99]}}}},
                                "m": {"max_bucket": {"buckets_path": "t>p"}}, "m2": {"min_bucket": {"buckets_path": "t>p.p"}}})
    add("pipe_single_percent_rank_hdr", {"t": {"terms": {"field": "k"}, "aggs": {"p": {"percentile_ranks": {"field": "n", "values": [40]}}, "h": {"percentiles": {"field": "n", "percents": [50], "hdr": {}}}}},
                                         "m": {"max_bucket": {"buckets_path": "t>p"}}, "m2": {"sum_bucket": {"buckets_path": "t>h"}}})
    add("pipe_two_percents_no_key", {"t": {"terms": {"field": "k"}, "aggs": {"p": {"percentiles": {"field": "n", "percents": [99, 50]}}}}, "m": {"max_bucket": {"buckets_path": "t>p"}}})
    add("terms_order_single_percent", {"t": {"terms": {"field": "k", "order": {"p": "desc"}}, "aggs": {"p": {"percentiles": {"field": "n", "percents": [99]}}}}})
    add("terms_order_single_percent_name", {"t": {"terms": {"field": "k", "order": {"p.p": "desc"}}, "aggs": {"p": {"percentiles": {"field": "n", "percents": [99]}}}}})
    add("composite_ip_bool", {"c": {"composite": {"size": 4, "sources": [{"ip": {"terms": {"field": "ip"}}}, {"b": {"terms": {"field": "b"}}}], "after": {"ip": "10.0.0.20", "b": False}}}})
    return c



def random_tree_cases(n, seed, wide=False):
    """Random aggregation trees over the main mapping (bucket aggs nested up to three deep, with
    metrics and pipelines), for differential runs against Elasticsearch. `wide` adds more
    aggregation types and parent pipelines."""
    rnd = random.Random(seed)
    num_fields = ["n", "i", "d", "f", "t", "b"]
    kw_fields = ["k", "tag", "ip", "nest.y"]

    def metric(depth_fields, nested):
        f = rnd.choice(["nest.x"] if nested else ["n", "i", "d", "f"])
        t = rnd.choice(["avg", "sum", "min", "max", "value_count", "stats", "extended_stats", "cardinality", "percentiles", "median_absolute_deviation", "weighted_avg", "top_metrics", "boxplot", "string_stats"])
        if nested and t == "top_metrics":
            t = "max"  # a nested sort field needs a nested sort context
        if t == "cardinality":
            return {"cardinality": {"field": rnd.choice(["nest.y"] if nested else ["k", "tag", "n", "ip"]), "precision_threshold": rnd.choice([1, 10, 100, 3000])}}
        if t == "percentiles":
            return {"percentiles": {"field": f, "percents": [rnd.choice([1, 25, 50, 90, 99.9])], "tdigest": {"compression": rnd.choice([20, 100, 300])}}}
        if t == "weighted_avg":
            return {"weighted_avg": {"value": {"field": f}, "weight": {"field": "i" if not nested else "nest.x", "missing": 1}}}
        if t == "top_metrics" and not nested:
            # Size 1 only: a sibling top_metrics of size > 1 under terms corrupts other
            # TDigests of the request in Elasticsearch 9.5.5 (a recycled-array bug).
            rnd.choice([1, 2])
            return {"top_metrics": {"metrics": {"field": f}, "sort": {"i": rnd.choice(["asc", "desc"])}, "size": 1}}
        if t == "string_stats":
            return {"string_stats": {"field": "nest.y" if nested else rnd.choice(["k", "tag"])}}
        if t == "extended_stats":
            return {"extended_stats": {"field": f, "sigma": rnd.choice([1, 2, 3.5])}}
        return {t: {"field": f}}

    def bucket(depth, nested):
        choices = ["terms", "terms", "histogram", "date_histogram", "range", "filters", "filter", "missing", "multi_terms", "rare_terms", "significant_terms"]
        if not nested:
            choices += ["nested", "composite_like"]
        if wide:
            choices += ["ip_range", "date_range", "adjacency_matrix", "geohash_grid", "geotile_grid", "auto_date_histogram", "variable_width_histogram",
                        "composite", "geo_distance", "ip_prefix", "sampler", "diversified_sampler", "global", "reverse_nested"]
        t = rnd.choice(choices)
        if t == "ip_range" and not nested:
            return "ip_range", {"field": "ip", "ranges": [{"to": "10.0.0.10"}, {"mask": "10.0.0.0/28"}, {"from": "192.168.1.3"}], "keyed": rnd.random() < 0.3}
        if t == "date_range" and not nested:
            return "date_range", {"field": "t", "ranges": [{"to": "2020-04-01"}, {"from": "2020-02-01", "to": "2020-11-15"}, {"from": "2020-11-15"}],
                                  "time_zone": rnd.choice(["UTC", "Europe/Paris"]), "format": rnd.choice(["yyyy-MM-dd", "strict_date_optional_time"])}
        if t == "adjacency_matrix" and not nested:
            return "adjacency_matrix", {"filters": {"x": {"term": {"b": True}}, "y": {"range": {"n": {"lt": 40}}}, "w": {"range": {"i": {"gte": 0}}}}}
        if t in ("geohash_grid", "geotile_grid") and not nested:
            return t, {"field": "g", "precision": rnd.choice([1, 2, 3]), "size": rnd.choice([3, 10]), "shard_size": rnd.choice([3, 20])}
        if t == "auto_date_histogram" and not nested and depth > 0:
            return "auto_date_histogram", {"field": "t", "buckets": rnd.choice([3, 7, 20])}
        if t == "variable_width_histogram" and not nested and depth == 0:
            return "variable_width_histogram", {"field": rnd.choice(["n", "d"]), "buckets": rnd.choice([3, 6])}
        if t == "composite" and not nested and depth == 0:
            # Not the multi-valued n: queries are ranges on n, and Elasticsearch then reads a
            # leading numeric source from the points inside the query range only
            # (PointsSortedDocsProducer), dropping a doc's other values.
            src = [{"a": {"terms": {"field": rnd.choice(["k", "i", "b"]), "missing_bucket": rnd.random() < 0.3}}}]
            if rnd.random() < 0.5:
                src.append({"h": {"histogram": {"field": "i", "interval": 25, "order": rnd.choice(["asc", "desc"])}}})
            return "composite", {"sources": src, "size": rnd.choice([2, 5, 20])}
        if t == "geo_distance" and not nested:
            return "geo_distance", {"field": "g", "origin": "10, 20", "unit": "km", "ranges": [{"to": 3000}, {"from": 3000, "to": 8000}, {"from": 8000}]}
        if t == "ip_prefix" and not nested:
            return "ip_prefix", {"field": "ip", "prefix_length": rnd.choice([8, 24, 30])}
        if t == "sampler" and not nested:
            return "sampler", {"shard_size": rnd.choice([5, 50])}
        if t == "diversified_sampler" and not nested:
            return "diversified_sampler", {"shard_size": rnd.choice([5, 50]), "field": rnd.choice(["tag", "i", "i", "k"])}
        if t == "global" and depth == 0:
            return "global", {}
        if t == "reverse_nested" and nested:
            return "reverse_nested", {}
        if t == "terms":
            f = rnd.choice(["nest.y", "nest.x"] if nested else ["k", "tag", "n", "i", "ip", "b", "t", "d"])
            body = {"field": f, "size": rnd.choice([1, 2, 3, 5, 10])}
            if rnd.random() < 0.4:
                body["shard_size"] = rnd.choice([1, 2, 4, 25])
            if rnd.random() < 0.5:
                body["show_term_doc_count_error"] = True
            if rnd.random() < 0.2:
                body["min_doc_count"] = rnd.choice([0, 2, 10])
            o = rnd.random()
            if o < 0.2:
                body["order"] = {"_key": rnd.choice(["asc", "desc"])}
            elif o < 0.35:
                body["order"] = {"_count": "asc"}
            return "terms", body
        if t == "histogram":
            f = "nest.x" if nested else rnd.choice(["n", "i", "d", "f"])
            body = {"field": f, "interval": rnd.choice([1, 2.5, 7, 10, 25])}
            if rnd.random() < 0.3:
                body["min_doc_count"] = rnd.choice([0, 1, 3])
            if rnd.random() < 0.2:
                body["order"] = {"_count": "desc"}
            return "histogram", body
        if t == "date_histogram" and not nested:
            body = {"field": "t"}
            if rnd.random() < 0.5:
                body["calendar_interval"] = rnd.choice(["month", "quarter", "week", "year"])
            else:
                body["fixed_interval"] = rnd.choice(["30d", "7d", "90d"])
            if rnd.random() < 0.4:
                body["time_zone"] = rnd.choice(["Europe/Paris", "America/New_York", "+05:30", "Asia/Tokyo"])
            if rnd.random() < 0.3:
                body["min_doc_count"] = rnd.choice([0, 5])
            return "date_histogram", body
        if t == "range":
            f = "nest.x" if nested else rnd.choice(["n", "d", "i"])
            cuts = sorted(rnd.sample(range(-20, 100), 3))
            return "range", {"field": f, "ranges": [{"to": cuts[0]}, {"from": cuts[0], "to": cuts[2]}, {"from": cuts[1]}], "keyed": rnd.random() < 0.3}
        if t == "filters":
            return "filters", {"filters": {"a": {"term": {"b": True}}, "z": {"range": {"n": {"lt": 30}}}}, "other_bucket": rnd.random() < 0.5}
        if t == "filter":
            return "filter", {"range": {("nest.x" if nested else "n"): {"gte": rnd.choice([2, 20, 50])}}}
        if t == "missing":
            return "missing", {"field": rnd.choice(["nest.x"] if nested else ["d", "k", "t", "g"])}
        if t == "multi_terms" and not nested:
            return "multi_terms", {"terms": [{"field": "k"}, {"field": rnd.choice(["b", "n"])}], "size": rnd.choice([2, 5]), "show_term_doc_count_error": rnd.random() < 0.5}
        if t == "rare_terms":
            return "rare_terms", {"field": "nest.y" if nested else rnd.choice(["tag", "n"]), "max_doc_count": rnd.choice([1, 2])}
        if t == "significant_terms" and not nested and depth > 0:
            return "significant_terms", {"field": rnd.choice(["k", "tag"]), "min_doc_count": 1, "size": 3}
        if t == "nested":
            return "nested", {"path": "nest"}
        return "terms", {"field": "nest.y" if nested else "k", "size": 3}

    def tree(depth, nested, parent_multi, terms_above=False):
        aggs = {}
        for j in range(rnd.choice([1, 1, 2, 3])):
            if depth < 3 and rnd.random() < 0.55:
                t, body = bucket(depth, nested)
                sub_nested = nested or t == "nested"
                node = {t: body}
                # Elasticsearch's rare_terms hands its sub-aggregations the wrong buckets when
                # it drops no term (see rare_terms.rs); which buckets it was asked for depends on
                # how terms ancestors executed (breadth_first, filters), which isn't modelled.
                want_subs = t not in ("rare_terms",) or rnd.random() < 0.5
                if t == "rare_terms" and terms_above:
                    want_subs = False
                # Elasticsearch 9.5.5's adjacency_matrix with sub-aggregations under another
                # bucket aggregation corrupts the parent's buckets (doc counts and keys).
                if t == "adjacency_matrix" and depth > 0:
                    want_subs = False
                if want_subs:
                    if t == "reverse_nested":
                        sub_nested = False
                    subs = tree(depth + 1, sub_nested, t in ("terms", "histogram", "date_histogram", "range", "filters", "multi_terms"),
                                terms_above or t in ("terms", "multi_terms", "significant_terms", "composite"))
                    if subs:
                        node["aggs"] = subs
                aggs["b%d%d" % (depth, j)] = node
            else:
                aggs["m%d%d" % (depth, j)] = metric(depth, nested)
        # Pipelines over the metrics of this level.
        metrics = [k for k, v in aggs.items() if list(v)[0] in ("avg", "sum", "min", "max", "value_count", "cardinality")]
        if parent_multi and metrics and rnd.random() < 0.3:
            m = rnd.choice(metrics)
            aggs["p%d" % depth] = rnd.choice([
                {"bucket_sort": {"sort": [{m: {"order": rnd.choice(["asc", "desc"])}}], "size": 2}},
                {"bucket_selector": {"buckets_path": {"v": "_count"}, "script": "params.v > 5"}},
            ])
        histos = [k for k, v in aggs.items() if list(v)[0] in ("histogram", "date_histogram") and "aggs" in v]
        if histos and rnd.random() < 0.5:
            h = rnd.choice(histos)
            inner = [k for k, v in aggs[h]["aggs"].items() if list(v)[0] in ("avg", "sum", "min", "max", "value_count")]
            target = h + ">" + (inner[0] if inner else "_count")
            aggs["s%d" % depth] = {rnd.choice(["avg_bucket", "max_bucket", "sum_bucket", "stats_bucket", "percentiles_bucket"]): {"buckets_path": target}}
            if inner and rnd.random() < 0.6:
                aggs[h]["aggs"]["dv"] = {"derivative": {"buckets_path": inner[0]}}
                aggs[h]["aggs"]["cs"] = {"cumulative_sum": {"buckets_path": inner[0]}}
                if wide:
                    gp = rnd.choice(["skip", "insert_zeros", "keep_values"])
                    aggs[h]["aggs"]["mf"] = {"moving_fn": {"buckets_path": inner[0], "window": rnd.choice([2, 5]), "shift": rnd.choice([0, 1]), "gap_policy": gp,
                                                           "script": rnd.choice(["MovingFunctions.unweightedAvg(values)", "MovingFunctions.linearWeightedAvg(values)",
                                                                                 "MovingFunctions.ewma(values, 0.4)", "MovingFunctions.holt(values, 0.5, 0.2)",
                                                                                 "MovingFunctions.stdDev(values, MovingFunctions.unweightedAvg(values))",
                                                                                 "MovingFunctions.max(values)", "MovingFunctions.min(values)", "MovingFunctions.sum(values)"])}}
                    aggs[h]["aggs"]["sd"] = {"serial_diff": {"buckets_path": inner[0], "lag": rnd.choice([1, 3]), "gap_policy": gp}}
                    aggs[h]["aggs"]["nz"] = {"normalize": {"buckets_path": inner[0], "method": rnd.choice(["rescale_0_1", "rescale_0_100", "percent_of_sum", "mean", "z-score", "softmax"])}}
                    aggs[h]["aggs"]["bs"] = {"bucket_sort": {"from": rnd.choice([0, 1]), "size": rnd.choice([3, 50]), "gap_policy": gp}}
        return aggs

    c = []
    for k in range(n):
        body = {"aggs": tree(0, False, False)}
        r = rnd.random()
        if r < 0.3:
            body["query"] = {"range": {"n": {"gte": rnd.randrange(0, 60)}}}
        elif r < 0.4:
            body["query"] = {"term": {"b": rnd.random() < 0.5}}
        c.append({"name": "rnd_%03d" % k, "body": json.dumps(body, separators=(",", ":")), "typed_keys": rnd.random() < 0.3})
    return c



def review_fix_cases():
    """Malformed aggregation paths, extreme parameters (NaN compressions, Long.MIN/MAX bounds and
    offsets) and the other inputs a mutation fuzzer found: errors must match, nothing may panic."""
    c = []

    def add(name, aggs, query=None, typed_keys=False):
        body = {"aggs": aggs}
        if query is not None:
            body["query"] = query
        c.append({"name": name, "body": json.dumps(body, separators=(",", ":")), "typed_keys": typed_keys})

    LMIN, LMAX = -9223372036854775808, 9223372036854775807
    bad_paths = {"empty": "", "gt": ">", "gt_spaced": " > ", "gtgt": ">>", "dot": ".", "trailing_dot": "a.",
                 "open_key": "h['x", "empty_key": "h[]>a"}
    # Orders.
    for pn, path in bad_paths.items():
        add("order_terms_" + pn, {"t": {"terms": {"field": "k", "order": {path: "asc"}}, "aggs": {"a": {"avg": {"field": "n"}}}}})
    add("order_terms_gt_top_metrics", {"t": {"terms": {"field": "k", "order": {">": "asc"}}, "aggs": {"tm": {"top_metrics": {"metrics": {"field": "d"}, "sort": {"n": "asc"}}}}}})
    add("order_terms_gt_stats", {"t": {"terms": {"field": "k", "order": {">": "desc"}}, "aggs": {"s": {"stats": {"field": "n"}}}}})
    add("order_terms_empty_compound", {"t": {"terms": {"field": "k", "order": [{"_count": "desc"}, {"": "asc"}]}}})
    add("order_multi_terms_empty", {"t": {"multi_terms": {"terms": [{"field": "k"}, {"field": "tag"}], "order": {"": "asc"}}}})
    add("order_histogram_empty", {"h": {"histogram": {"field": "n", "interval": 10, "order": {"": "asc"}}}})
    add("order_date_histogram_gt", {"h": {"date_histogram": {"field": "t", "calendar_interval": "month", "order": {">": "asc"}}}})
    add("order_rare_terms_like_ok", {"t": {"terms": {"field": "k", "order": {"a": "asc"}}, "aggs": {"a": {"avg": {"field": "n"}}}}})
    # Sibling pipelines.
    histo = {"histogram": {"field": "n", "interval": 10}, "aggs": {"a": {"avg": {"field": "d"}}}}
    for pt in ["max_bucket", "avg_bucket", "stats_bucket", "extended_stats_bucket", "percentiles_bucket"]:
        for pn, path in list(bad_paths.items()) + [("array_empty", []), ("array_two", ["h>a", "_count"]), ("array_two_bad", ["a", "_count"]),
                                                   ("array_one", ["h>a"])]:
            if pt != "max_bucket" and pn not in ("empty", "gt", "array_empty", "dot"):
                continue
            add("sibling_%s_%s" % (pt, pn), {"h": histo, "m": {pt: {"buckets_path": path}}})
            if pn in ("empty", "gt", "array_empty", "dot"):
                add("sibling_inner_%s_%s" % (pt, pn), {"h": {"histogram": {"field": "n", "interval": 10},
                                                              "aggs": {"t": {"terms": {"field": "k"}, "aggs": {"a": {"avg": {"field": "d"}}}},
                                                                       "m": {pt: {"buckets_path": path}}}}})
    add("sibling_empty_array_first_error", {"m": {"max_bucket": {"buckets_path": []}}, "d": {"derivative": {"buckets_path": "x"}}})
    add("sibling_empty_array_bad_percents", {"h": histo, "m": {"percentiles_bucket": {"buckets_path": [], "percents": [101]}}})
    add("sibling_composite", {"c": {"composite": {"sources": [{"k": {"terms": {"field": "k"}}}]}, "aggs": {"s": {"sum": {"field": "n"}}}},
                              "m": {"max_bucket": {"buckets_path": "c>s"}}})
    add("sibling_composite_count", {"c": {"composite": {"sources": [{"k": {"terms": {"field": "k"}}}]}}, "m": {"avg_bucket": {"buckets_path": "c>_count"}}})
    add("sibling_composite_under_filter", {"f": {"filter": {"exists": {"field": "n"}}, "aggs": {"c": {"composite": {"sources": [{"k": {"terms": {"field": "k"}}}]}, "aggs": {"s": {"sum": {"field": "n"}}}}}},
                                           "m": {"max_bucket": {"buckets_path": "f>c>s"}}})
    add("sibling_filter_no_subs", {"f": {"filter": {"exists": {"field": "n"}}}, "m": {"max_bucket": {"buckets_path": "f"}}})
    add("sibling_filter_terms_ok", {"f": {"filter": {"exists": {"field": "n"}}, "aggs": {"t": {"terms": {"field": "k"}, "aggs": {"s": {"sum": {"field": "n"}}}}}},
                                    "m": {"max_bucket": {"buckets_path": "f>t>s"}}})
    # Parent pipelines.
    for pt in ["derivative", "serial_diff", "cumulative_sum", "cumulative_cardinality", "moving_fn", "normalize"]:
        for pn, path in [("array_empty", []), ("empty", ""), ("gt", ">"), ("dot", "."), ("array_two", ["a", "_count"]), ("array_one", ["a"])]:
            body = {"buckets_path": path}
            if pt == "moving_fn":
                body.update({"window": 2, "script": "MovingFunctions.max(values)"})
            if pt == "normalize":
                body["method"] = "mean"
            if pt == "serial_diff":
                body["lag"] = 1
            sub = {"a": {"avg": {"field": "d"}}, "p": {pt: body}}
            if pt == "cumulative_cardinality":
                sub["a"] = {"cardinality": {"field": "k"}}
            add("parent_%s_%s" % (pt, pn), {"h": {"histogram": {"field": "n", "interval": 10, "min_doc_count": 0}, "aggs": sub}})
    add("parent_serial_diff_empty_under_terms", {"t": {"terms": {"field": "k"}, "aggs": {"s1": {"serial_diff": {"buckets_path": []}}}}})
    for pn, path in [("empty", ""), ("gt", ">"), ("dot", ".")]:
        add("bucket_selector_" + pn, {"t": {"terms": {"field": "k"}, "aggs": {"a": {"avg": {"field": "n"}},
                                                                            "s": {"bucket_selector": {"buckets_path": {"x": path}, "script": "params.x > 0"}}}}})
        add("bucket_script_" + pn, {"t": {"terms": {"field": "k"}, "aggs": {"a": {"avg": {"field": "n"}},
                                                                          "s": {"bucket_script": {"buckets_path": {"x": path}, "script": "params.x"}}}}})
        add("bucket_sort_" + pn, {"t": {"terms": {"field": "k"}, "aggs": {"a": {"avg": {"field": "n"}},
                                                                        "s": {"bucket_sort": {"sort": [{path: "asc"}]}}}}})
    # TDigest compressions.
    for comp in ["NaN", 0, 1e-300]:
        cn = str(comp).replace(".", "_").replace("-", "m")
        add("tdigest_percentiles_" + cn, {"p": {"percentiles": {"field": "d", "tdigest": {"compression": comp}}}})
        add("tdigest_ranks_" + cn, {"p": {"percentile_ranks": {"field": "d", "values": [10, 50], "tdigest": {"compression": comp}}}})
        add("tdigest_mad_" + cn, {"p": {"median_absolute_deviation": {"field": "d", "compression": comp}}})
        add("tdigest_boxplot_" + cn, {"p": {"boxplot": {"field": "d", "compression": comp}}})
    add("tdigest_nan_no_values", {"p": {"percentiles": {"field": "d", "tdigest": {"compression": "NaN"}}}}, query={"term": {"k": "none"}})
    add("tdigest_nan_in_terms", {"t": {"terms": {"field": "k"}, "aggs": {"p": {"percentiles": {"field": "d", "tdigest": {"compression": "NaN"}}}}}})
    # Cardinality on geo_point.
    for pt in [None, 0, 1, 100, 40000]:
        body = {"field": "g"}
        if pt is not None:
            body["precision_threshold"] = pt
        add("cardinality_geo_%s" % pt, {"c": {"cardinality": body}})
    add("cardinality_geo_missing", {"c": {"cardinality": {"field": "g", "missing": "10.5,20.25"}}, "c2": {"cardinality": {"field": "g", "missing": [20.25, 10.5]}}})
    add("cardinality_geo_in_terms", {"t": {"terms": {"field": "k"}, "aggs": {"c": {"cardinality": {"field": "g"}}}}})
    add("cardinality_geo_in_histogram", {"h": {"histogram": {"field": "n", "interval": 25}, "aggs": {"c": {"cardinality": {"field": "g", "precision_threshold": 0}}}}})
    # Long overflow in date roundings, offsets and bounds.
    dh = lambda **kw: {"h": {"date_histogram": dict({"field": "t"}, **kw)}}
    add("overflow_offset_min", dh(fixed_interval="1d", offset="-9223372036854775807ms"))
    add("overflow_offset_max", dh(fixed_interval="1d", offset="9223372036854775807ms"))
    add("overflow_offset_max_hour_zone", dh(fixed_interval="3h", offset="9223372036854775807ms", time_zone="+05:30"))
    add("overflow_ext_min_auckland", dh(calendar_interval="day", time_zone="Pacific/Auckland", min_doc_count=0, extended_bounds={"min": LMIN, "max": "2020-10-05"}, format="yyyy-MM-dd"))
    add("overflow_ext_max_auckland", dh(calendar_interval="day", time_zone="Pacific/Auckland", min_doc_count=0, extended_bounds={"min": "2019-12-30", "max": LMAX}, format="yyyy-MM-dd"))
    add("overflow_ext_max_day", dh(calendar_interval="day", min_doc_count=0, extended_bounds={"min": "2019-12-30", "max": LMAX}))
    add("overflow_ext_max_fixed", dh(fixed_interval="1d", min_doc_count=0, extended_bounds={"min": "2019-12-30", "max": LMAX}))
    add("overflow_ext_min_fixed", dh(fixed_interval="1d", min_doc_count=0, extended_bounds={"min": LMIN, "max": "2019-12-30"}))
    add("overflow_ext_min_month", dh(calendar_interval="month", min_doc_count=0, extended_bounds={"min": LMIN, "max": "2019-12-30"}))
    add("overflow_ext_max_year_zone", dh(calendar_interval="year", time_zone="-03:00", min_doc_count=0, extended_bounds={"min": "2019-12-30", "max": LMAX}))
    add("overflow_ext_max_week", dh(calendar_interval="week", min_doc_count=0, extended_bounds={"min": "2019-12-30", "max": LMAX}))
    add("overflow_missing_min_7d", dh(fixed_interval="7d", missing=LMIN))
    add("overflow_missing_max_7d", dh(fixed_interval="7d", missing=LMAX))
    add("overflow_missing_min_month", dh(calendar_interval="month", missing=LMIN))
    add("overflow_missing_max_day_zone", dh(calendar_interval="day", time_zone="Europe/Paris", missing=LMAX))
    add("overflow_far_bounds_1000000d", dh(fixed_interval="1000000d", min_doc_count=0, extended_bounds={"min": -8000000000000000000, "max": 8000000000000000000}))
    add("overflow_offset_max_month_bounds", dh(calendar_interval="month", offset=LMAX, extended_bounds={"min": "2019-06-01", "max": "2021-09-01"}))
    add("overflow_offset_max_month", dh(calendar_interval="month", offset=LMAX))
    add("overflow_offset_min_month", dh(calendar_interval="month", offset=-LMAX))
    add("overflow_hard_bounds", dh(calendar_interval="day", hard_bounds={"min": LMIN, "max": LMAX}))
    add("overflow_hard_bounds_fixed", dh(fixed_interval="1h", hard_bounds={"min": LMIN, "max": LMAX}))
    add("overflow_date_range_epoch_second_max", {"r": {"date_range": {"field": "t", "format": "epoch_second", "ranges": [{"to": LMAX}, {"from": 1590000000, "to": "1600000000"}]}}})
    add("overflow_date_range_epoch_second_min", {"r": {"date_range": {"field": "t", "format": "epoch_second", "ranges": [{"from": LMIN}]}}})
    add("overflow_date_range_epoch_second_big", {"r": {"date_range": {"field": "t", "format": "epoch_second", "ranges": [{"to": 9223372036854775}, {"from": 9223372036854776}]}}})
    add("overflow_date_range_millis_max", {"r": {"date_range": {"field": "t", "ranges": [{"to": LMAX}, {"from": LMIN}]}}})
    add("overflow_histogram_ext_far", {"h": {"histogram": {"field": "n", "interval": 1, "min_doc_count": 0, "extended_bounds": {"min": -1e300, "max": 5}}}})
    add("overflow_histogram_offset_far", {"h": {"histogram": {"field": "n", "interval": 10, "offset": 9.2e18}}})
    # (auto_date_histogram with missing Long.MAX kills Elasticsearch's JVM: not captured.)
    add("overflow_composite_date_offset", {"c": {"composite": {"sources": [{"d": {"date_histogram": {"field": "t", "fixed_interval": "1d", "offset": "-9223372036854775807ms"}}}]}}})
    # Unmapped terms with value_type and missing.
    for vt, miss in [("long", 3), ("double", 3), ("long", "3"), ("date", 3), ("ip", "1.2.3.4"), ("boolean", True), ("string", 3), ("numeric", 3)]:
        add("terms_unmapped_value_type_%s_%s" % (vt, type(miss).__name__), {"t": {"terms": {"field": "nope", "value_type": vt, "missing": miss}}}, typed_keys=True)
    add("terms_unmapped_value_type_long_sub", {"t": {"terms": {"field": "nope", "value_type": "long", "missing": 7}, "aggs": {"x": {"terms": {"field": "nope2", "value_type": "double", "missing": 1.5}}}}}, typed_keys=True)
    add("terms_long_missing_fraction", {"t": {"terms": {"field": "n", "missing": 1.5}}, "u": {"terms": {"field": "d", "missing": 3}}}, typed_keys=True)
    add("terms_unmapped_missing_fraction", {"t": {"terms": {"field": "nope", "missing": 1.5, "value_type": "long"}}}, typed_keys=True)
    add("rare_terms_long_missing_fraction", {"t": {"rare_terms": {"field": "n", "missing": 1.5}}})
    add("sig_terms_unmapped_missing_keyword", {"t": {"significant_terms": {"field": "nope", "missing": "x"}}})
    add("sig_terms_long_missing_fraction", {"t": {"significant_terms": {"field": "n", "missing": 1.5}}})
    add("rare_terms_unmapped_value_type_long", {"t": {"rare_terms": {"field": "nope", "value_type": "long", "missing": 3}}}, typed_keys=True)
    add("sig_terms_unmapped_value_type_long", {"t": {"significant_terms": {"field": "nope", "value_type": "long", "missing": 3}}}, typed_keys=True)
    # top_metrics size.
    add("top_metrics_size_0", {"tm": {"top_metrics": {"metrics": {"field": "d"}, "sort": {"i": "asc"}, "size": 0}}})
    add("top_metrics_size_neg", {"tm": {"top_metrics": {"metrics": {"field": "d"}, "sort": {"i": "asc"}, "size": -1}}})
    add("top_metrics_size_0_first", {"tm": {"top_metrics": {"size": 0, "metrics": {"field": "d"}, "sort": {"i": "asc"}}}})
    add("top_metrics_size_0_no_sort", {"tm": {"top_metrics": {"metrics": {"field": "d"}, "size": 0}}})
    return c


def random_date_cases(n, seed):
    """Random date_histogram / date_range / auto_date_histogram / composite date sources over
    the main docs: every calendar and fixed unit, time zones with DST and odd offsets,
    `offset`, bounds, `min_doc_count`, formats and keyed output."""
    rnd = random.Random(seed)
    zones = ["UTC", "Europe/Paris", "America/New_York", "Asia/Kathmandu", "Australia/Lord_Howe", "Pacific/Chatham", "-03:30", "+05:45",
             "America/Sao_Paulo", "Europe/London", "Asia/Tehran", "America/St_Johns", "Africa/Casablanca"]
    cal = ["minute", "hour", "day", "week", "month", "quarter", "year", "1d", "1M", "1w", "1h", "1q", "1y"]
    fixed = ["7d", "30d", "12h", "90m", "3600s", "5d", "1000000ms", "36h"]
    fmts = [None, None, "yyyy-MM-dd", "yyyy-MM-dd'T'HH:mm:ssXXX", "epoch_second", "date_time", "MMM yyyy", "EEE, dd MMM yyyy HH:mm", "week_date", "ordinal_date"]
    c = []
    for k in range(n):
        aggs = {}
        for j in range(rnd.choice([1, 2])):
            body = {"field": "t"}
            if rnd.random() < 0.5:
                unit = rnd.choice(cal)
                body["calendar_interval"] = unit
            else:
                unit = rnd.choice(fixed)
                body["fixed_interval"] = unit
            if rnd.random() < 0.7:
                body["time_zone"] = rnd.choice(zones)
            if rnd.random() < 0.3:
                body["offset"] = rnd.choice(["+1h", "-6h", "+2d", "-30m", "+1d", "15m"])
            f = rnd.choice(fmts)
            if f:
                body["format"] = f
            if rnd.random() < 0.4:
                body["min_doc_count"] = rnd.choice([0, 1, 3])
            # Keyed output with a format coarser than the interval repeats keys, which a JSON
            # object (and so this fixture) can't hold.
            if rnd.random() < 0.2 and not f:
                body["keyed"] = True
            if rnd.random() < 0.2:
                body["order"] = rnd.choice([{"_count": "desc"}, {"_key": "desc"}])
            fine = unit in ("minute", "hour", "1h", "90m", "3600s", "1000000ms", "12h", "36h")
            if fine:
                lo = 1585000000000 + rnd.randrange(0, 200) * 86400000
                query_range = (lo, lo + rnd.choice([1, 3, 7]) * 86400000)
            else:
                query_range = None
            if body.get("min_doc_count") == 0 and rnd.random() < 0.6:
                if fine:
                    body["extended_bounds"] = {"min": query_range[0] - 7200000, "max": query_range[1] + 7200000}
                elif f:
                    # Bound strings parse with the aggregation's format: give numbers.
                    body["extended_bounds"] = {"min": 1559347200000, "max": 1633046400000}
                else:
                    body["extended_bounds"] = {"min": "2019-06-01", "max": "2021-09-30"}
            if rnd.random() < 0.15 and "extended_bounds" not in body:
                if fine:
                    body["hard_bounds"] = {"min": query_range[0] + 3600000, "max": query_range[1] - 3600000}
                else:
                    body["hard_bounds"] = {"min": 1581292800000, "max": 1605830400000} if f else {"min": "2020-02-10", "max": "2020-11-20"}
            sub = {}
            if rnd.random() < 0.4:
                sub["m"] = {rnd.choice(["max", "min", "avg", "sum"]): {"field": rnd.choice(["n", "d", "t"])}}
            if rnd.random() < 0.2 and (body.get("min_doc_count") == 0 or rnd.random() < 0.1):
                sub["c"] = {"cumulative_sum": {"buckets_path": "_count"}}
            node = {"date_histogram": body}
            if sub:
                node["aggs"] = sub
            aggs["h%d" % j] = node
        if rnd.random() < 0.3:
            tz = rnd.choice(zones)
            aggs["r"] = {"date_range": {"field": "t", "time_zone": tz, "format": "strict_date_optional_time",
                                        "ranges": [{"to": "2020-03-29T02:30"}, {"from": "2020-03-01", "to": "2020-10-25T01:30"}, {"from": "2020-10-25"}]}}
        if rnd.random() < 0.2:
            aggs["a"] = {"auto_date_histogram": {"field": "t", "buckets": rnd.choice([3, 10, 40]), "time_zone": rnd.choice(zones)}}
        if rnd.random() < 0.2:
            src = {"date_histogram": {"field": "t", "calendar_interval": rnd.choice(["month", "week", "day"]), "time_zone": rnd.choice(zones)}}
            aggs["cp"] = {"composite": {"size": 7, "sources": [{"d": src}]}}
        body = {"aggs": aggs}
        fine_q = [a["date_histogram"] for a in aggs.values() if "date_histogram" in a and (a["date_histogram"].get("calendar_interval") in ("minute", "hour", "1h") or a["date_histogram"].get("fixed_interval") in ("90m", "3600s", "1000000ms", "12h", "36h"))]
        if fine_q:
            lo = 1585000000000 + rnd.randrange(0, 200) * 86400000
            body["query"] = {"range": {"t": {"gte": lo, "lt": lo + 3 * 86400000}}}
        c.append({"name": "date_%03d" % k, "body": json.dumps(body, separators=(",", ":")), "typed_keys": rnd.random() < 0.2})
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
        if os.environ.get("FIXTURE_VERBOSE"):
            print(c["name"], file=sys.stderr, flush=True)
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
        # The random suites are big: one case per line keeps them diffable and small.
        if suite.startswith("rnd"):
            head = {k: v for k, v in fixture.items() if k != "cases"}
            f.write(json.dumps(head, separators=(",", ":"))[:-1] + ',"cases":[\n')
            f.write(",\n".join(json.dumps(c, separators=(",", ":")) for c in fixture["cases"]))
            f.write("\n]}\n")
        else:
            json.dump(fixture, f, indent=1)
            f.write("\n")
    print(suite, len(out_cases), "cases")



def multi_index():
    """Two one-shard indices whose mappings differ (a field mapped in one, unmapped or of
    another type in the other), searched together: the reduce must pick its leader, formats and
    types the way Elasticsearch does. Writes aggs-mi2.json."""
    indices = [
        ("aggs-mi-b", {"properties": {"d": {"type": "date"}, "ip": {"type": "ip"}, "n": {"type": "long"}, "k": {"type": "keyword"}}},
         [{"d": 1619827200000, "ip": "192.168.0.1", "n": 5, "k": "x"}, {"d": 1577836800000, "ip": "10.0.0.1", "n": 7, "k": "y"}]),
        ("aggs-mi-a", {"properties": {"n": {"type": "double"}, "k": {"type": "keyword"}, "other": {"type": "long"}}},
         [{"n": 1.5, "k": "x", "other": 1}, {"n": 7, "k": "z", "other": 2}, {"other": 3}]),
    ]
    # Searched shards reduce in index-name order: the index without the date and ip fields
    # (aggs-mi-a) comes first.
    indices.sort(key=lambda x: x[0])
    for name, mapping, docs in indices:
        call("DELETE", "/" + name)
        r = call("PUT", "/" + name, {"settings": {"number_of_shards": 1, "number_of_replicas": 0}, "mappings": mapping})
        assert r.get("acknowledged"), r
        lines = []
        for i, d in enumerate(docs):
            lines.append(json.dumps({"index": {"_id": str(i)}}))
            lines.append(json.dumps(d))
        call("POST", "/" + name + "/_bulk?refresh=true", "\n".join(lines) + "\n", ndjson=True)
        call("POST", "/" + name + "/_forcemerge?max_num_segments=1")
    cases = []

    def add(cname, aggs, typed_keys=False):
        cases.append({"name": cname, "body": json.dumps({"aggs": aggs}, separators=(",", ":")), "typed_keys": typed_keys})

    add("max_min_date_unmapped_first", {"mx": {"max": {"field": "d"}}, "mn": {"min": {"field": "d"}}, "a": {"avg": {"field": "d"}}, "s": {"stats": {"field": "d"}}}, True)
    add("composite_ip_unmapped", {"c": {"composite": {"sources": [{"f": {"terms": {"field": "ip"}}}]}}})
    add("composite_ip_and_long", {"c": {"composite": {"sources": [{"f": {"terms": {"field": "ip"}}}, {"o": {"terms": {"field": "other", "missing_bucket": True}}}]}}})
    add("terms_long_double", {"t": {"terms": {"field": "n"}}}, True)
    add("terms_ip_unmapped", {"t": {"terms": {"field": "ip"}}, "o": {"terms": {"field": "other"}}}, True)
    add("histogram_date_unmapped", {"h": {"date_histogram": {"field": "d", "calendar_interval": "year"}}, "r": {"range": {"field": "other", "ranges": [{"to": 2}, {"from": 2}]}}}, True)
    add("cardinality_mixed", {"c": {"cardinality": {"field": "n"}}, "k": {"cardinality": {"field": "k"}}, "p": {"percentiles": {"field": "n", "percents": [50]}}})
    add("rare_and_sig_unmapped", {"r": {"rare_terms": {"field": "ip"}}, "t": {"terms": {"field": "k"}, "aggs": {"m": {"max": {"field": "d"}}}}})
    out = []
    for c in cases:
        q = "/aggs-mi-a,aggs-mi-b/_search?size=0" + ("&typed_keys=true" if c["typed_keys"] else "")
        r = call("POST", q, c["body"])
        e = dict(c)
        if "error" in r:
            e["error"] = r["error"]
        else:
            e["response"] = r.get("aggregations")
        out.append(e)
    for name, _, _ in indices:
        call("DELETE", "/" + name)
    fixture = {"indices": [{"name": n, "mapping": m, "docs": d} for n, m, d in indices], "cases": out}
    with open(os.path.join(HERE, "aggs-mi2.json"), "w") as f:
        json.dump(fixture, f, indent=1)
        f.write("\n")
    print("mi2", len(out), "cases")


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
    "catr1": lambda: generate("catr1", CATR_MAPPING, catr_docs(0, 41), catr_cases(), 1),
    "cat2": lambda: generate("cat2", CAT_MAPPING, cat_docs(600, 37), cat_cases(), 2),
    "fis2": lambda: generate("fis2", FIS_MAPPING, fis_docs(500, 31), fis_cases(), 2),
    "lic2": lambda: generate("lic2", LIC_MAPPING, lic_docs(600, 23), lic_cases(), 2),
    "err3": lambda: generate("err3", MAIN_MAPPING, main_docs(400, 42), core_err_cases(), 3),
    "rnd3": lambda: generate("rnd3", MAIN_MAPPING, main_docs(400, 42), random_tree_cases(300, 7), 3),
    "mi2": multi_index,
    "rndw3": lambda: generate("rndw3", MAIN_MAPPING, main_docs(400, 42), random_tree_cases(300, 11, wide=True), 3),
    "rnddate2": lambda: generate("rnddate2", MAIN_MAPPING, main_docs(400, 42), random_date_cases(250, 5), 2),
    "rndx2": lambda: generate("rndx2", MAIN_MAPPING, main_docs(400, 43), random_tree_cases(300, 29, wide=True), 2),
    "fix3": lambda: generate("fix3", MAIN_MAPPING, main_docs(400, 42), review_fix_cases(), 3),
    "tsg2": lambda: generate("tsg2", TSG_MAPPING, tsg_docs(600, 29), tsg_cases(), 2, TSG_SETTINGS),
}

if __name__ == "__main__":
    names = sys.argv[1:] or list(SUITES)
    for n in names:
        SUITES[n]()
