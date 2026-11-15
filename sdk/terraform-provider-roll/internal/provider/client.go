package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// client speaks roll secrets' HTTP API (docs/secrets.md, "HTTP API"), as the CLI does.
type client struct {
	api     string
	token   string
	project string
	http    *http.Client
}

// apiError is an answer that wasn't a success. It never holds a value.
type apiError struct {
	status  int
	code    string
	message string
}

func (e *apiError) Error() string { return fmt.Sprintf("%s (%d)", e.message, e.status) }

func notFound(err error) bool {
	e, ok := err.(*apiError)
	return ok && e.status == http.StatusNotFound
}

func newClient(api, token, project string) *client {
	return &client{api: strings.TrimRight(api, "/"), token: token, project: project, http: &http.Client{Timeout: 60 * time.Second}}
}

func (c *client) call(ctx context.Context, method, path string, body, out any) error {
	var r io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		r = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.api+path, r)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+c.token)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	res, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	data, err := io.ReadAll(io.LimitReader(res.Body, 16<<20))
	if err != nil {
		return err
	}
	if res.StatusCode >= 300 {
		var e struct {
			Error string `json:"error"`
			Code  string `json:"code"`
		}
		_ = json.Unmarshal(data, &e)
		if e.Error == "" {
			e.Error = res.Status
		}
		return &apiError{status: res.StatusCode, code: e.Code, message: e.Error}
	}
	if out == nil || len(data) == 0 {
		return nil
	}
	return json.Unmarshal(data, out)
}

func (c *client) envPath(project, env string) string {
	return "/v1/secrets/projects/" + url.PathEscape(project) + "/environments/" + url.PathEscape(env)
}

func pathQuery(path string) string {
	if path == "" {
		path = "/"
	}
	return "?path=" + url.QueryEscape(path)
}

type environment struct {
	Slug      string `json:"slug"`
	Name      string `json:"name"`
	Protected bool   `json:"protected"`
}

type link struct {
	Kind  string `json:"kind"`
	Ref   string `json:"ref"`
	Field string `json:"field"`
	Mode  string `json:"mode"`
}

type secret struct {
	Key     string `json:"key"`
	Path    string `json:"path"`
	Version int64  `json:"version"`
	Comment string `json:"comment"`
	Link    *link  `json:"link"`
	Source  struct {
		Kind string `json:"kind"`
	} `json:"source"`
}
