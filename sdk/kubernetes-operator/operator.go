// The roll secrets Kubernetes operator (docs/secrets.md, "Kubernetes"): each RollSecret names a
// project's environment and folder, and the operator keeps a Secret beside it in step. It logs in
// with its own service account token through a workload identity, so roll holds no cluster
// credential, and it writes only Secrets it made (owned by their RollSecret, so they go with it).
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/roll-sh/roll/sdk/go/secrets"
)

const (
	group   = "roll.sh"
	version = "v1"
	// Secrets the operator made carry this label, and their RollSecret as controller.
	managedLabel = "roll.sh/managed-by"
	managedValue = "rollsecret"
)

type operator struct {
	kube *kube
	api  string
	// The identity a RollSecret that names none logs in as (ROLL_IDENTITY).
	identity string
	// A roll token instead of an identity (ROLL_TOKEN): for clusters roll can't verify.
	token string
	// The workload's own OIDC token (a projected service account token whose audience is the API).
	oidc func() (string, error)
	now  func() time.Time

	mu      sync.Mutex
	clients map[string]*secrets.Client // by identity
}

// client is a logged-in client for identity, kept until roll refuses it (its token ended).
func (o *operator) client(ctx context.Context, identity string) (*secrets.Client, error) {
	if identity == "" && o.token != "" {
		return secrets.New(o.api, o.token), nil
	}
	if identity == "" {
		return nil, errors.New("no identity: set spec.identity or ROLL_IDENTITY")
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	if c := o.clients[identity]; c != nil {
		return c, nil
	}
	oidc, err := o.oidc()
	if err != nil {
		return nil, err
	}
	c, err := secrets.Login(ctx, o.api, identity, oidc)
	if err != nil {
		return nil, err
	}
	o.clients[identity] = c
	return c, nil
}

func (o *operator) forget(identity string) {
	o.mu.Lock()
	defer o.mu.Unlock()
	delete(o.clients, identity)
}

func refused(err error) bool {
	var e *secrets.Error
	return errors.As(err, &e) && e.Status == http.StatusUnauthorized
}

// ownedBy whether s is rs's: made by the operator for this very RollSecret.
func ownedBy(s *secretObj, rs *rollSecret) bool {
	if s.Metadata.Labels[managedLabel] != managedValue {
		return false
	}
	for _, r := range s.Metadata.OwnerReferences {
		if r.UID == rs.Metadata.UID && r.Kind == "RollSecret" {
			return true
		}
	}
	return false
}

func secretPath(ns, name string) string {
	return "/api/v1/namespaces/" + url.PathEscape(ns) + "/secrets/" + url.PathEscape(name)
}

// reconcile brings rs's Secret in step with its environment: the status it should have.
func (o *operator) reconcile(ctx context.Context, rs *rollSecret) rollStatus {
	st := rs.Status
	st.ObservedGeneration = rs.Metadata.Generation
	fail := func(err error) rollStatus {
		st.Error = err.Error()
		return st
	}
	identity := rs.Spec.Identity
	if identity == "" {
		identity = o.identity
	}
	if rs.Spec.Environment == "" {
		return fail(errors.New("spec.environment is required"))
	}
	name := rs.Spec.SecretName
	if name == "" {
		name = rs.Metadata.Name
	}
	ns := rs.Metadata.Namespace

	// The digest moves when the values would: nothing else is read while it holds.
	var c *secrets.Client
	var digest string
	for attempt := 0; ; attempt++ {
		login, err := o.client(ctx, identity)
		if err != nil {
			return fail(fmt.Errorf("logging in to roll: %w", err))
		}
		c = login
		if rs.Spec.Project != "" {
			c = &secrets.Client{API: login.API, Token: login.Token, Project: rs.Spec.Project, HTTP: login.HTTP}
		}
		digest, err = c.Digest(ctx, rs.Spec.Environment, rs.Spec.Path)
		if refused(err) && attempt == 0 {
			o.forget(identity) // its short-lived token ended: log in again
			continue
		}
		if err != nil {
			return fail(err)
		}
		break
	}

	var existing *secretObj
	var s secretObj
	switch err := o.kube.call(ctx, http.MethodGet, secretPath(ns, name), "", nil, &s); {
	case err == nil:
		existing = &s
	case isStatus(err, http.StatusNotFound):
	default:
		return fail(err)
	}
	if existing != nil && !ownedBy(existing, rs) {
		return fail(fmt.Errorf("Secret %s/%s exists and isn't this RollSecret's: it isn't touched", ns, name))
	}
	if existing != nil && digest == rs.Status.Digest && rs.Status.Error == "" {
		return st
	}

	v, err := c.Values(ctx, rs.Spec.Environment, rs.Spec.Path)
	if err != nil {
		return fail(err)
	}
	if len(v.Problems) > 0 {
		return fail(fmt.Errorf("%s doesn't resolve: %s: %s (the Secret keeps its values)", rs.Spec.Environment, v.Problems[0].Key, v.Problems[0].Problem))
	}
	data := make(map[string][]byte, len(v.Values))
	for k, val := range v.Values {
		data[k] = []byte(val)
	}
	want := secretObj{
		APIVersion: "v1",
		Kind:       "Secret",
		Type:       "Opaque",
		Data:       data,
		Metadata: meta{
			Name:      name,
			Namespace: ns,
			Labels:    map[string]string{managedLabel: managedValue},
			OwnerReferences: []ownerRef{{
				APIVersion: group + "/" + version, Kind: "RollSecret", Name: rs.Metadata.Name, UID: rs.Metadata.UID, Controller: true, BlockOwnerDeletion: true,
			}},
		},
	}
	if existing != nil {
		want.Metadata.ResourceVersion = existing.Metadata.ResourceVersion
		err = o.kube.call(ctx, http.MethodPut, secretPath(ns, name), "application/json", want, nil)
	} else {
		err = o.kube.call(ctx, http.MethodPost, "/api/v1/namespaces/"+url.PathEscape(ns)+"/secrets", "application/json", want, nil)
	}
	if err != nil {
		return fail(err)
	}
	return rollStatus{Digest: digest, SyncedAt: o.now().UTC().Format(time.RFC3339), Keys: len(data), ObservedGeneration: rs.Metadata.Generation}
}

// pass reconciles every RollSecret in the cluster once.
func (o *operator) pass(ctx context.Context) error {
	var list struct {
		Items []rollSecret `json:"items"`
	}
	if err := o.kube.call(ctx, http.MethodGet, "/apis/"+group+"/"+version+"/rollsecrets", "", nil, &list); err != nil {
		return err
	}
	for i := range list.Items {
		rs := &list.Items[i]
		st := o.reconcile(ctx, rs)
		if st == rs.Status {
			continue
		}
		if st.Error != "" {
			log.Printf("rollsecret %s/%s: %s", rs.Metadata.Namespace, rs.Metadata.Name, st.Error)
		}
		path := "/apis/" + group + "/" + version + "/namespaces/" + url.PathEscape(rs.Metadata.Namespace) + "/rollsecrets/" + url.PathEscape(rs.Metadata.Name) + "/status"
		if err := o.kube.call(ctx, http.MethodPatch, path, "application/merge-patch+json", map[string]any{"status": st}, nil); err != nil {
			log.Printf("rollsecret %s/%s: writing its status: %v", rs.Metadata.Namespace, rs.Metadata.Name, err)
		}
	}
	return nil
}

func main() {
	env := func(k string) string { return strings.TrimSpace(os.Getenv(k)) }
	k, err := kubeFromEnv()
	if err != nil {
		log.Fatal(err)
	}
	every := 30 * time.Second
	if s := env("ROLL_SYNC_EVERY"); s != "" {
		if every, err = time.ParseDuration(s); err != nil {
			log.Fatalf("ROLL_SYNC_EVERY: %v", err)
		}
	}
	o := &operator{
		kube:     k,
		api:      env("ROLL_API_URL"),
		identity: env("ROLL_IDENTITY"),
		token:    env("ROLL_TOKEN"),
		now:      time.Now,
		clients:  map[string]*secrets.Client{},
		oidc: func() (string, error) {
			b, err := os.ReadFile(env("ROLL_OIDC_TOKEN_FILE"))
			return strings.TrimSpace(string(b)), err
		},
	}
	if o.api == "" {
		log.Fatal("ROLL_API_URL isn't set")
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, os.Interrupt)
	defer stop()
	log.Printf("roll secrets operator: every %s", every)
	for {
		if err := o.pass(ctx); err != nil {
			log.Printf("listing RollSecrets: %v", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(every):
		}
	}
}
