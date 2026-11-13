package main

import (
	"bytes"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"
	"time"
)

// kube is the Kubernetes API over plain HTTP: in a cluster, the service account's own token
// (re-read on every request: it rotates) and the cluster's CA; KUBE_API and KUBE_TOKEN elsewhere.
type kube struct {
	api       string
	tokenFile string
	token     string
	http      *http.Client
}

const serviceAccount = "/var/run/secrets/kubernetes.io/serviceaccount"

func kubeFromEnv() (*kube, error) {
	if api := os.Getenv("KUBE_API"); api != "" {
		return &kube{api: strings.TrimRight(api, "/"), token: os.Getenv("KUBE_TOKEN"), http: &http.Client{Timeout: 30 * time.Second}}, nil
	}
	host, port := os.Getenv("KUBERNETES_SERVICE_HOST"), os.Getenv("KUBERNETES_SERVICE_PORT")
	if host == "" {
		return nil, fmt.Errorf("not in a cluster: set KUBE_API")
	}
	ca, err := os.ReadFile(serviceAccount + "/ca.crt")
	if err != nil {
		return nil, err
	}
	pool := x509.NewCertPool()
	pool.AppendCertsFromPEM(ca)
	t := &http.Transport{TLSClientConfig: &tls.Config{RootCAs: pool, MinVersion: tls.VersionTLS12}}
	return &kube{api: "https://" + host + ":" + port, tokenFile: serviceAccount + "/token", http: &http.Client{Timeout: 30 * time.Second, Transport: t}}, nil
}

type kubeError struct {
	status int
	msg    string
}

func (e *kubeError) Error() string { return fmt.Sprintf("kubernetes: %d %s", e.status, e.msg) }

func isStatus(err error, status int) bool {
	e, ok := err.(*kubeError)
	return ok && e.status == status
}

func (k *kube) call(ctx context.Context, method, path, contentType string, body, out any) error {
	var r io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		r = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, k.api+path, r)
	if err != nil {
		return err
	}
	token := k.token
	if k.tokenFile != "" {
		b, err := os.ReadFile(k.tokenFile)
		if err != nil {
			return err
		}
		token = strings.TrimSpace(string(b))
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	if body != nil {
		req.Header.Set("Content-Type", contentType)
	}
	req.Header.Set("Accept", "application/json")
	res, err := k.http.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	data, err := io.ReadAll(io.LimitReader(res.Body, 32<<20))
	if err != nil {
		return err
	}
	if res.StatusCode >= 300 {
		var s struct {
			Message string `json:"message"`
		}
		_ = json.Unmarshal(data, &s)
		return &kubeError{status: res.StatusCode, msg: s.Message}
	}
	if out == nil || len(data) == 0 {
		return nil
	}
	return json.Unmarshal(data, out)
}

// The objects the operator reads and writes, as far as it needs them.

type meta struct {
	Name            string            `json:"name"`
	Namespace       string            `json:"namespace,omitempty"`
	UID             string            `json:"uid,omitempty"`
	ResourceVersion string            `json:"resourceVersion,omitempty"`
	Generation      int64             `json:"generation,omitempty"`
	Labels          map[string]string `json:"labels,omitempty"`
	OwnerReferences []ownerRef        `json:"ownerReferences,omitempty"`
}

type ownerRef struct {
	APIVersion         string `json:"apiVersion"`
	Kind               string `json:"kind"`
	Name               string `json:"name"`
	UID                string `json:"uid"`
	Controller         bool   `json:"controller"`
	BlockOwnerDeletion bool   `json:"blockOwnerDeletion"`
}

type rollSecret struct {
	Metadata meta `json:"metadata"`
	Spec     struct {
		// The project (prj_…); the identity's own when left out.
		Project     string `json:"project"`
		Environment string `json:"environment"`
		// The folder (default /).
		Path string `json:"path"`
		// The Secret to keep in step, in the RollSecret's namespace (default: its own name).
		SecretName string `json:"secretName"`
		// The workload identity to log in as (default ROLL_IDENTITY).
		Identity string `json:"identity"`
	} `json:"spec"`
	Status rollStatus `json:"status"`
}

type rollStatus struct {
	Digest             string `json:"digest,omitempty"`
	SyncedAt           string `json:"syncedAt,omitempty"`
	Keys               int    `json:"keys"`
	Error              string `json:"error,omitempty"`
	ObservedGeneration int64  `json:"observedGeneration,omitempty"`
}

type secretObj struct {
	APIVersion string            `json:"apiVersion"`
	Kind       string            `json:"kind"`
	Metadata   meta              `json:"metadata"`
	Type       string            `json:"type,omitempty"`
	Data       map[string][]byte `json:"data"`
}
