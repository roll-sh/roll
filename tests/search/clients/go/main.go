// go-elasticsearch v9 against roll search: the shared client scenario through the typed API
// and the esutil.BulkIndexer helper, plus the client's product check (it refuses servers
// without the X-Elastic-Product header on the first response).
//
// Usage: client   (ES_URL, default http://127.0.0.1:59200)
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"sync/atomic"
	"time"

	"github.com/elastic/go-elasticsearch/v9"
	"github.com/elastic/go-elasticsearch/v9/esutil"
	"github.com/elastic/go-elasticsearch/v9/typedapi/core/search"
	"github.com/elastic/go-elasticsearch/v9/typedapi/types"
	"github.com/elastic/go-elasticsearch/v9/typedapi/types/enums/calendarinterval"
	"github.com/elastic/go-elasticsearch/v9/typedapi/types/enums/refresh"
	"github.com/elastic/go-elasticsearch/v9/typedapi/types/enums/sortorder"
)

const idx = "clients-go"

var colors = []string{"red", "green", "blue"}
var failures []string
var ctx = context.Background()

func check(name string, fn func() error) {
	if err := fn(); err != nil {
		failures = append(failures, name)
		fmt.Printf("FAIL %s: %v\n", name, err)
		return
	}
	fmt.Printf("ok   %s\n", name)
}

func eq[T comparable](got, want T, what string) error {
	if got != want {
		return fmt.Errorf("%s: expected %v, got %v", what, want, got)
	}
	return nil
}

func doc(i int) map[string]any {
	title := "lazy dog"
	if i%2 == 1 {
		title = "quick brown fox"
	}
	return map[string]any{
		"title":    fmt.Sprintf("document %d %s", i, title),
		"tag":      colors[i%3],
		"n":        i,
		"price":    float64(i) * 1.5,
		"d":        fmt.Sprintf("2024-%02d-%02d", i%12+1, i%28+1),
		"comments": []map[string]any{{"author": fmt.Sprintf("a%d", i%5), "stars": i % 5}},
	}
}

func main() {
	url := os.Getenv("ES_URL")
	if url == "" {
		url = "http://127.0.0.1:59200"
	}
	cfg := elasticsearch.Config{Addresses: []string{url}}
	es, err := elasticsearch.NewTypedClient(cfg)
	if err != nil {
		panic(err)
	}
	low, err := elasticsearch.NewClient(cfg)
	if err != nil {
		panic(err)
	}

	check("info and product check", func() error {
		info, err := es.Info().Do(ctx)
		if err != nil {
			return err
		}
		if err := eq(info.Version.Int, "9.5.5", "version"); err != nil {
			return err
		}
		ok, err := es.Ping().IsSuccess(ctx)
		if err != nil {
			return err
		}
		return eq(ok, true, "ping")
	})

	check("create index with mappings", func() error {
		_, _ = es.Indices.Delete(idx + "*").Do(ctx)
		shards, replicas := "2", "0"
		r, err := es.Indices.Create(idx).Settings(&types.IndexSettings{NumberOfShards: &shards, NumberOfReplicas: &replicas}).
			Mappings(&types.TypeMapping{Properties: map[string]types.Property{
				"title":    types.TextProperty{Fields: map[string]types.Property{"raw": types.NewKeywordProperty()}},
				"tag":      types.NewKeywordProperty(),
				"n":        types.NewIntegerNumberProperty(),
				"price":    types.NewDoubleNumberProperty(),
				"d":        types.NewDateProperty(),
				"comments": types.NestedProperty{Properties: map[string]types.Property{"author": types.NewKeywordProperty(), "stars": types.NewIntegerNumberProperty()}},
			}}).Do(ctx)
		if err != nil {
			return err
		}
		return eq(r.Acknowledged, true, "acknowledged")
	})

	check("BulkIndexer helper", func() error {
		bi, err := esutil.NewBulkIndexer(esutil.BulkIndexerConfig{Client: low, Index: idx, NumWorkers: 3, FlushBytes: 2048})
		if err != nil {
			return err
		}
		var failed atomic.Int64
		for i := 0; i < 100; i++ {
			body, _ := json.Marshal(doc(i))
			err := bi.Add(ctx, esutil.BulkIndexerItem{Action: "index", DocumentID: fmt.Sprint(i), Body: bytes.NewReader(body),
				OnFailure: func(context.Context, esutil.BulkIndexerItem, esutil.BulkIndexerResponseItem, error) { failed.Add(1) }})
			if err != nil {
				return err
			}
		}
		if err := bi.Close(ctx); err != nil {
			return err
		}
		st := bi.Stats()
		if err := eq(st.NumIndexed, uint64(100), "indexed"); err != nil {
			return err
		}
		if err := eq(failed.Load(), int64(0), "failed"); err != nil {
			return err
		}
		if _, err := es.Indices.Refresh().Index(idx).Do(ctx); err != nil {
			return err
		}
		c, err := es.Count().Index(idx).Do(ctx)
		if err != nil {
			return err
		}
		return eq(c.Count, int64(100), "count")
	})

	check("search with query and aggregations", func() error {
		gte, lt := types.Float64(10), types.Float64(60)
		field := "tag"
		nfield, pfield, dfield, sfield := "n", "price", "d", "comments.stars"
		path := "comments"
		r, err := es.Search().Index(idx).Request(&search.Request{
			Query: &types.Query{Bool: &types.BoolQuery{
				Must:   []types.Query{{Match: map[string]types.MatchQuery{"title": {Query: "fox"}}}},
				Filter: []types.Query{{Range: map[string]types.RangeQuery{"n": types.NumberRangeQuery{Gte: &gte, Lt: &lt}}}},
			}},
			Aggregations: map[string]types.Aggregations{
				"tags":   {Terms: &types.TermsAggregation{Field: &field}, Aggregations: map[string]types.Aggregations{"avg_n": {Avg: &types.AverageAggregation{Field: &nfield}}}},
				"months": {DateHistogram: &types.DateHistogramAggregation{Field: &dfield, CalendarInterval: &calendarinterval.Month}},
				"price":  {Stats: &types.StatsAggregation{Field: &pfield}},
				"stars":  {Nested: &types.NestedAggregation{Path: &path}, Aggregations: map[string]types.Aggregations{"max": {Max: &types.MaxAggregation{Field: &sfield}}}},
			},
			Size: ptr(5),
			Sort: []types.SortCombinations{types.SortOptions{SortOptions: map[string]types.FieldSort{"n": {Order: &sortorder.Asc}}}},
			Highlight: &types.Highlight{Fields: []map[string]types.HighlightField{{"title": {}}}},
		}).Do(ctx)
		if err != nil {
			return err
		}
		if err := eq(r.Hits.Total.Value, int64(25), "total"); err != nil {
			return err
		}
		ids := []string{}
		for _, h := range r.Hits.Hits {
			ids = append(ids, *h.Id_)
		}
		if err := eq(strings.Join(ids, ","), "11,13,15,17,19", "ids"); err != nil {
			return err
		}
		if err := eq(r.Hits.Hits[0].Highlight["title"][0], "document 11 quick brown <em>fox</em>", "highlight"); err != nil {
			return err
		}
		terms := r.Aggregations["tags"].(*types.StringTermsAggregate)
		got := map[string]string{}
		for _, b := range terms.Buckets.([]types.StringTermsBucket) {
			got[b.Key.(string)] = fmt.Sprintf("%d/%v", b.DocCount, *b.Aggregations["avg_n"].(*types.AvgAggregate).Value)
		}
		if err := eq(fmt.Sprint(got), "map[blue:9/35 green:8/34 red:8/36]", "terms"); err != nil {
			return err
		}
		if err := eq(r.Aggregations["price"].(*types.StatsAggregate).Count, int64(25), "stats.count"); err != nil {
			return err
		}
		max := r.Aggregations["stars"].(*types.NestedAggregate).Aggregations["max"].(*types.MaxAggregate)
		if err := eq(float64(*max.Value), 4.0, "nested max"); err != nil {
			return err
		}
		total := int64(0)
		for _, b := range r.Aggregations["months"].(*types.DateHistogramAggregate).Buckets.([]types.DateHistogramBucket) {
			total += b.DocCount
		}
		return eq(total, int64(25), "date_histogram")
	})

	check("documents: get, update, conflicts, delete", func() error {
		g, err := es.Get(idx, "5").Do(ctx)
		if err != nil {
			return err
		}
		var src map[string]any
		_ = json.Unmarshal(g.Source_, &src)
		if err := eq(src["n"].(float64), 5.0, "get"); err != nil {
			return err
		}
		ok, err := es.Exists(idx, "nope").IsSuccess(ctx)
		if err != nil {
			return err
		}
		if err := eq(ok, false, "exists"); err != nil {
			return err
		}
		u, err := es.Update(idx, "5").Doc(map[string]any{"n": 500}).Refresh(refresh.True).Do(ctx)
		if err != nil {
			return err
		}
		if err := eq(u.Result.String(), "updated", "update"); err != nil {
			return err
		}
		_, err = es.Index(idx).Id("5").Document(map[string]any{"n": 1}).IfSeqNo("0").IfPrimaryTerm("1").Do(ctx)
		var ee *types.ElasticsearchError
		if !errors.As(err, &ee) {
			return fmt.Errorf("expected a version conflict, got %v", err)
		}
		if err := eq(ee.Status, 409, "status"); err != nil {
			return err
		}
		if err := eq(ee.ErrorCause.Type, "version_conflict_engine_exception", "type"); err != nil {
			return err
		}
		if _, err := es.Index(idx).Id("tmp").Document(map[string]any{"n": -1}).Refresh(refresh.Waitfor).Do(ctx); err != nil {
			return err
		}
		d, err := es.Delete(idx, "tmp").Refresh(refresh.True).Do(ctx)
		if err != nil {
			return err
		}
		return eq(d.Result.String(), "deleted", "delete")
	})

	check("scroll", func() error {
		r, err := es.Search().Index(idx).Scroll("1m").Size(7).Do(ctx)
		if err != nil {
			return err
		}
		seen := len(r.Hits.Hits)
		sid := *r.ScrollId_
		for {
			s, err := es.Scroll().ScrollId(sid).Scroll(duration("1m")).Do(ctx)
			if err != nil {
				return err
			}
			if len(s.Hits.Hits) == 0 {
				break
			}
			seen += len(s.Hits.Hits)
			sid = *s.ScrollId_
		}
		if err := eq(seen, 100, "scrolled"); err != nil {
			return err
		}
		c, err := es.ClearScroll().ScrollId(sid).Do(ctx)
		if err != nil {
			return err
		}
		return eq(c.Succeeded, true, "clear")
	})

	check("point in time with search_after", func() error {
		p, err := es.OpenPointInTime(idx).KeepAlive("1m").Do(ctx)
		if err != nil {
			return err
		}
		id := p.Id
		seen := map[string]bool{}
		var after []types.FieldValue
		for {
			req := &search.Request{Pit: &types.PointInTimeReference{Id: id, KeepAlive: "1m"}, Size: ptr(30),
				Sort: []types.SortCombinations{types.SortOptions{SortOptions: map[string]types.FieldSort{"n": {Order: &sortorder.Asc}}}}, SearchAfter: after}
			r, err := es.Search().Request(req).Do(ctx)
			if err != nil {
				return err
			}
			if len(r.Hits.Hits) == 0 {
				break
			}
			for _, h := range r.Hits.Hits {
				seen[*h.Id_] = true
			}
			after = r.Hits.Hits[len(r.Hits.Hits)-1].Sort
			id = *r.PitId
		}
		if err := eq(len(seen), 100, "paged"); err != nil {
			return err
		}
		c, err := es.ClosePointInTime().Id(id).Do(ctx)
		if err != nil {
			return err
		}
		return eq(c.Succeeded, true, "close")
	})

	check("update_by_query and delete_by_query", func() error {
		src := "ctx._source.n += 1000"
		u, err := es.UpdateByQuery(idx).Query(&types.Query{Term: map[string]types.TermQuery{"tag": {Value: "blue"}}}).
			Script(&types.Script{Source: src}).Refresh(true).Do(ctx)
		if err != nil {
			return err
		}
		if err := eq(*u.Updated, int64(33), "updated"); err != nil {
			return err
		}
		d, err := es.DeleteByQuery(idx).Query(&types.Query{Term: map[string]types.TermQuery{"tag": {Value: "green"}}}).Refresh(true).Do(ctx)
		if err != nil {
			return err
		}
		if err := eq(*d.Deleted, int64(33), "deleted"); err != nil {
			return err
		}
		c, err := es.Count().Index(idx).Do(ctx)
		if err != nil {
			return err
		}
		return eq(c.Count, int64(67), "count")
	})

	check("errors", func() error {
		_, err := es.Search().Index("clients-go-missing").Do(ctx)
		var ee *types.ElasticsearchError
		if !errors.As(err, &ee) {
			return fmt.Errorf("expected an error, got %v", err)
		}
		if err := eq(ee.Status, 404, "status"); err != nil {
			return err
		}
		return eq(ee.ErrorCause.Type, "index_not_found_exception", "type")
	})

	check("low-level client and cat", func() error {
		res, err := low.Cat.Indices(low.Cat.Indices.WithIndex(idx+"*"), low.Cat.Indices.WithFormat("json"), low.Cat.Indices.WithH("index"))
		if err != nil {
			return err
		}
		defer res.Body.Close()
		var rows []map[string]string
		if err := json.NewDecoder(res.Body).Decode(&rows); err != nil {
			return err
		}
		if err := eq(len(rows), 1, "rows"); err != nil {
			return err
		}
		return eq(rows[0]["index"], idx, "index")
	})

	check("delete index", func() error {
		r, err := es.Indices.Delete(idx + "*").Do(ctx)
		if err != nil {
			return err
		}
		return eq(r.Acknowledged, true, "acknowledged")
	})

	_ = time.Second
	if len(failures) > 0 {
		fmt.Printf("go-elasticsearch: %d check(s) failed: %s\n", len(failures), strings.Join(failures, ", "))
		os.Exit(1)
	}
	fmt.Println("go-elasticsearch: all checks passed")
}

func ptr[T any](v T) *T { return &v }

// duration is a types.DurationVariant: the typed API takes durations as a union type.
type duration string

func (d duration) DurationCaster() *types.Duration {
	var v types.Duration = string(d)
	return &v
}
