package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
)

// extensions is the Lambda Extensions API (https://docs.aws.amazon.com/lambda/latest/dg/runtimes-extensions-api.html).
type extensions struct {
	base string // http://${AWS_LAMBDA_RUNTIME_API}/2020-01-01/extension
	http *http.Client
	id   string // Lambda-Extension-Identifier, from register
}

// event is what event/next answers.
type event struct {
	EventType      string `json:"eventType"` // INVOKE or SHUTDOWN
	DeadlineMs     int64  `json:"deadlineMs"`
	RequestID      string `json:"requestId"`
	ShutdownReason string `json:"shutdownReason"`
}

// register names the extension (its file name in /opt/extensions, as Lambda requires) and the
// events it follows.
func (x *extensions) register(ctx context.Context, name string) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, x.base+"/register", bytes.NewReader([]byte(`{"events":["INVOKE","SHUTDOWN"]}`)))
	if err != nil {
		return err
	}
	req.Header.Set("Lambda-Extension-Name", name)
	res, err := x.do(req)
	if err != nil {
		return fmt.Errorf("registering with the Extensions API: %w", err)
	}
	res.Body.Close()
	if x.id = res.Header.Get("Lambda-Extension-Identifier"); x.id == "" {
		return errors.New("the Extensions API registered the extension without an identifier")
	}
	return nil
}

// next waits for the next event (Lambda answers once the function's invocation starts, or at
// shutdown; the sandbox is frozen in between).
func (x *extensions) next(ctx context.Context) (event, error) {
	var ev event
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, x.base+"/event/next", nil)
	if err != nil {
		return ev, err
	}
	req.Header.Set("Lambda-Extension-Identifier", x.id)
	res, err := x.do(req)
	if err != nil {
		return ev, fmt.Errorf("waiting for the next event: %w", err)
	}
	defer res.Body.Close()
	if err := json.NewDecoder(io.LimitReader(res.Body, 1<<20)).Decode(&ev); err != nil {
		return ev, fmt.Errorf("the next event isn't JSON: %w", err)
	}
	return ev, nil
}

// fail reports an error in `init` (the function's init fails, naming it) or before an `exit`.
func (x *extensions) fail(ctx context.Context, phase, kind string, cause error) {
	body, _ := json.Marshal(map[string]string{"errorMessage": cause.Error(), "errorType": "Extension." + kind})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, x.base+"/"+phase+"/error", bytes.NewReader(body))
	if err != nil {
		return
	}
	req.Header.Set("Lambda-Extension-Identifier", x.id)
	req.Header.Set("Lambda-Extension-Function-Error-Type", "Extension."+kind)
	if res, err := x.do(req); err == nil {
		res.Body.Close()
	}
}

func (x *extensions) do(req *http.Request) (*http.Response, error) {
	res, err := x.http.Do(req)
	if err != nil {
		return nil, err
	}
	if res.StatusCode != http.StatusOK && res.StatusCode != http.StatusAccepted {
		b, _ := io.ReadAll(io.LimitReader(res.Body, 4<<10))
		res.Body.Close()
		return nil, fmt.Errorf("%s: %s", res.Status, bytes.TrimSpace(b))
	}
	return res, nil
}
