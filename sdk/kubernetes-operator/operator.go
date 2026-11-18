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
	// Namespaces whose RollSecrets may use the operator's own login ("*": every one). Elsewhere a
	// RollSecret logs in as a service account of its own namespace: the operator's login reaches
	// whatever its identity does, and creating a RollSecret mustn't be a way to it.
	namespaces map[string]bool

	mu      sync.Mutex
	clients map[string]*secrets.Client // by login (namespace/service account, identity)

	// Push: each RollSecret's digest is long-polled between passes, and a change wakes the loop.
	every   time.Duration
	wake    chan struct{}
	seen    map[string]watchTarget // what the last pass reconciled, by namespace/name
	watches map[string]*watchState
}

// What a RollSecret's watch long-polls: its environment and folder, as its login reads them,
// from the digest its Secret holds.
type watchTarget struct {
	c         *secrets.Client
	env, path string
	digest    string
}

type watchState struct {
	target watchTarget
	cancel context.CancelFunc
	done   chan struct{}
}

// observe records what a reconcile left a RollSecret at, for its watch.
func (o *operator) observe(rs *rollSecret, t watchTarget) {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.seen != nil {
		o.seen[rs.Metadata.Namespace+"/"+rs.Metadata.Name] = t
	}
}

// rearm keeps one watch per RollSecret the last pass reconciled: a new one where its client,
// folder or digest changed or the old one ended, none for RollSecrets that are gone.
func (o *operator) rearm(ctx context.Context) {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.watches == nil {
		o.watches = map[string]*watchState{}
	}
	if o.wake == nil {
		o.wake = make(chan struct{}, 1)
	}
	for key, w := range o.watches {
		t, ok := o.seen[key]
		ended := false
		select {
		case <-w.done:
			ended = true
		default:
		}
		if !ok || ended || t != w.target {
			w.cancel()
			delete(o.watches, key)
		}
	}
	for key, t := range o.seen {
		if _, ok := o.watches[key]; ok {
			continue
		}
		wctx, cancel := context.WithCancel(ctx)
		w := &watchState{target: t, cancel: cancel, done: make(chan struct{})}
		o.watches[key] = w
		go o.watchOne(wctx, w)
	}
}

// watchOne long-polls one RollSecret's digest until it moves (then wakes the loop) or the watch
// is cancelled. An error, or an answer that came back at once unchanged (roll holds at most 32
// polls per login), waits a full interval: never a busy loop.
func (o *operator) watchOne(ctx context.Context, w *watchState) {
	defer close(w.done)
	every := o.every
	if every <= 0 {
		every = 30 * time.Second
	}
	for ctx.Err() == nil {
		asked := time.Now()
		d, err := w.target.c.DigestSince(ctx, w.target.env, w.target.path, w.target.digest, 25*time.Second)
		if ctx.Err() != nil {
			return
		}
		if err == nil && d != w.target.digest {
			select {
			case o.wake <- struct{}{}:
			default:
			}
			return
		}
		if err != nil || time.Since(asked) < time.Second {
			select {
			case <-ctx.Done():
				return
			case <-time.After(every):
			}
		}
	}
}

// run reconciles every RollSecret, then again after the interval or as soon as a watch saw a
// change, until ctx ends.
func (o *operator) run(ctx context.Context) {
	o.mu.Lock()
	if o.wake == nil {
		o.wake = make(chan struct{}, 1)
	}
	o.mu.Unlock()
	every := o.every
	if every <= 0 {
		every = 30 * time.Second
	}
	for {
		if err := o.pass(ctx); err != nil {
			log.Printf("listing RollSecrets: %v", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(every):
		case <-o.wake:
		}
	}
}

// login is how a RollSecret logs in: as a service account of its namespace, or as the operator.
type login struct{ namespace, serviceAccount, identity string }

func (l login) key() string { return l.namespace + "/" + l.serviceAccount + "/" + l.identity }

// client is a logged-in client for l, kept until roll refuses it (its token ended).
func (o *operator) client(ctx context.Context, l login) (*secrets.Client, error) {
	if l.serviceAccount == "" && !o.namespaces[l.namespace] && !o.namespaces["*"] {
		return nil, fmt.Errorf("RollSecrets in %s log in as a service account of their own namespace: set spec.serviceAccountName (and spec.identity, an identity whose subject is system:serviceaccount:%s:<name>), or add %s to the operator's namespaces", l.namespace, l.namespace, l.namespace)
	}
	if l.serviceAccount == "" && l.identity == "" && o.token != "" {
		return secrets.New(o.api, o.token), nil
	}
	if l.identity == "" {
		return nil, errors.New("no identity: set spec.identity or ROLL_IDENTITY")
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	if c := o.clients[l.key()]; c != nil {
		return c, nil
	}
	var oidc string
	var err error
	if l.serviceAccount != "" {
		oidc, err = o.kube.serviceAccountToken(ctx, l.namespace, l.serviceAccount, o.api)
		if err != nil {
			return nil, fmt.Errorf("a token for service account %s/%s: %w", l.namespace, l.serviceAccount, err)
		}
	} else if oidc, err = o.oidc(); err != nil {
		return nil, err
	}
	c, err := secrets.Login(ctx, o.api, l.identity, oidc)
	if err != nil {
		return nil, err
	}
	o.clients[l.key()] = c
	return c, nil
}

func (o *operator) forget(l login) {
	o.mu.Lock()
	defer o.mu.Unlock()
	delete(o.clients, l.key())
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
	l := login{namespace: rs.Metadata.Namespace, serviceAccount: rs.Spec.ServiceAccountName, identity: rs.Spec.Identity}
	if l.identity == "" && l.serviceAccount == "" {
		l.identity = o.identity
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
		login, err := o.client(ctx, l)
		if err != nil {
			return fail(fmt.Errorf("logging in to roll: %w", err))
		}
		c = login
		if rs.Spec.Project != "" {
			c = &secrets.Client{API: login.API, Token: login.Token, Project: rs.Spec.Project, HTTP: login.HTTP}
		}
		digest, err = c.Digest(ctx, rs.Spec.Environment, rs.Spec.Path)
		if refused(err) && attempt == 0 {
			o.forget(l) // its short-lived token ended: log in again
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
	o.observe(rs, watchTarget{c: c, env: rs.Spec.Environment, path: rs.Spec.Path, digest: digest})
	if existing != nil && digest == rs.Status.Digest && rs.Status.Error == "" {
		if rs.Status.PendingRestarts != "" {
			st.PendingRestarts, st.RestartError = o.restart(ctx, ns, rs, pending(rs.Spec.RolloutRestart, rs.Status.PendingRestarts))
		}
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
	// A changed Secret restarts what reads it: only when its data changed (not a pass that clears an
	// earlier error, or a key rewritten with the same value), and never a new one (nothing read it).
	// Targets a change didn't restart yet are tried again on each pass, until they are.
	done := rollStatus{Digest: digest, SyncedAt: o.now().UTC().Format(time.RFC3339), Keys: len(data), ObservedGeneration: rs.Metadata.Generation}
	targets := pending(rs.Spec.RolloutRestart, rs.Status.PendingRestarts)
	if existing != nil && !sameData(existing.Data, data) {
		targets = rs.Spec.RolloutRestart
	}
	if len(targets) > 0 {
		done.PendingRestarts, done.RestartError = o.restart(ctx, ns, rs, targets)
	}
	return done
}

// pending is the targets of `all` named in a status's PendingRestarts.
func pending(all []restartTarget, names string) []restartTarget {
	var out []restartTarget
	for _, t := range all {
		for _, n := range strings.Split(names, ",") {
			if n == t.Kind+"/"+t.Name {
				out = append(out, t)
				break
			}
		}
	}
	return out
}

// sameData is whether a Secret's data is as it was.
func sameData(a, b map[string][]byte) bool {
	if len(a) != len(b) {
		return false
	}
	for k, v := range a {
		if w, ok := b[k]; !ok || string(v) != string(w) {
			return false
		}
	}
	return true
}

// restartKinds are the workloads a RollSecret may restart, by their API resource.
var restartKinds = map[string]string{"Deployment": "deployments", "StatefulSet": "statefulsets", "DaemonSet": "daemonsets"}

// restartOptIn is the annotation a workload opts in with: the RollSecrets (names, comma-separated)
// whose changes may restart it. Creating a RollSecret alone restarts nothing.
const restartOptIn = "roll.sh/restart-on"

// restart rolls each target's pods, as `kubectl rollout restart` does: a new annotation on the
// pod template. Only in the RollSecret's own namespace, and only a workload that names the
// RollSecret in its roll.sh/restart-on annotation. Every target is tried: the ones not restarted
// ("Kind/name,…") and why.
func (o *operator) restart(ctx context.Context, ns string, rs *rollSecret, targets []restartTarget) (string, string) {
	at := o.now().UTC().Format(time.RFC3339)
	var left, why []string
	for _, t := range targets {
		if err := o.restartOne(ctx, ns, rs.Metadata.Name, t, at); err != nil {
			left = append(left, t.Kind+"/"+t.Name)
			why = append(why, err.Error())
		}
	}
	return strings.Join(left, ","), strings.Join(why, "; ")
}

func (o *operator) restartOne(ctx context.Context, ns, rollSecret string, t restartTarget, at string) error {
	resource, ok := restartKinds[t.Kind]
	if !ok || t.Name == "" {
		return fmt.Errorf("spec.rolloutRestart: %s/%s isn't a Deployment, StatefulSet or DaemonSet", t.Kind, t.Name)
	}
	path := "/apis/apps/v1/namespaces/" + url.PathEscape(ns) + "/" + resource + "/" + url.PathEscape(t.Name)
	var w struct {
		Metadata meta `json:"metadata"`
	}
	if err := o.kube.call(ctx, http.MethodGet, path, "", nil, &w); err != nil {
		return fmt.Errorf("restarting %s %s (the Secret is updated): %w", t.Kind, t.Name, err)
	}
	optedIn := false
	for _, n := range strings.Split(w.Metadata.Annotations[restartOptIn], ",") {
		optedIn = optedIn || strings.TrimSpace(n) == rollSecret
	}
	if !optedIn {
		return fmt.Errorf("%s %s isn't restarted: its %s annotation doesn't name RollSecret %s", t.Kind, t.Name, restartOptIn, rollSecret)
	}
	patch := map[string]any{"spec": map[string]any{"template": map[string]any{"metadata": map[string]any{"annotations": map[string]string{"roll.sh/restartedAt": at}}}}}
	if err := o.kube.call(ctx, http.MethodPatch, path, "application/merge-patch+json", patch, nil); err != nil {
		return fmt.Errorf("restarting %s %s (the Secret is updated): %w", t.Kind, t.Name, err)
	}
	return nil
}

// pass reconciles every RollSecret in the cluster once.
func (o *operator) pass(ctx context.Context) error {
	var list struct {
		Items []rollSecret `json:"items"`
	}
	if err := o.kube.call(ctx, http.MethodGet, "/apis/"+group+"/"+version+"/rollsecrets", "", nil, &list); err != nil {
		return err
	}
	o.mu.Lock()
	o.seen = map[string]watchTarget{}
	o.mu.Unlock()
	defer o.rearm(ctx)
	for i := range list.Items {
		rs := &list.Items[i]
		st := o.reconcile(ctx, rs)
		if st == rs.Status {
			continue
		}
		if st.RestartError != "" {
			log.Printf("rollsecret %s/%s: %s", rs.Metadata.Namespace, rs.Metadata.Name, st.RestartError)
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
	namespaces := map[string]bool{}
	for _, n := range strings.Split(env("ROLL_OPERATOR_NAMESPACES"), ",") {
		if n = strings.TrimSpace(n); n != "" {
			namespaces[n] = true
		}
	}
	o := &operator{
		kube:       k,
		api:        env("ROLL_API_URL"),
		identity:   env("ROLL_IDENTITY"),
		token:      env("ROLL_TOKEN"),
		now:        time.Now,
		clients:    map[string]*secrets.Client{},
		namespaces: namespaces,
		every:      every,
		wake:       make(chan struct{}, 1),
		watches:    map[string]*watchState{},
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
	log.Printf("roll secrets operator: every %s, and within seconds of a change (each RollSecret's digest is long-polled)", every)
	o.run(ctx)
}
