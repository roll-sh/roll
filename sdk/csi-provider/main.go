// Command roll-csi-provider is roll secrets' provider for the Secrets Store CSI Driver
// (docs/secrets.md, "Kubernetes"): a pod's volume gets an environment's values as files, read when
// the pod starts (and on the driver's rotation), logged in with the pod's own service account. No
// Kubernetes Secret is made unless the SecretProviderClass asks the driver for one.
//
// It listens on a unix socket in the driver's provider directory (a DaemonSet, one per node).
package main

import (
	"context"
	"errors"
	"flag"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"
)

var version = "dev"

func main() {
	endpoint := flag.String("endpoint", "/provider/roll.sock", "the unix socket the driver connects to (in its provider directory)")
	flag.Parse()
	if err := os.MkdirAll(filepath.Dir(*endpoint), 0o755); err != nil {
		log.Fatalf("roll-csi-provider: %v", err)
	}
	// A socket left by the provider's previous process.
	if err := os.Remove(*endpoint); err != nil && !errors.Is(err, os.ErrNotExist) {
		log.Fatalf("roll-csi-provider: %v", err)
	}
	l, err := net.Listen("unix", *endpoint)
	if err != nil {
		log.Fatalf("roll-csi-provider: %v", err)
	}
	srv := serve(l, &provider{http: &http.Client{Timeout: 30 * time.Second}, version: version})
	log.Printf("roll-csi-provider %s listening on %s", version, *endpoint)
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM, syscall.SIGINT)
	<-stop
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_ = srv.Shutdown(ctx)
}

// serve answers the driver on l (gRPC: HTTP/2 without TLS, with prior knowledge) until the
// server shuts down.
func serve(l net.Listener, p providerServer) *http.Server {
	protocols := new(http.Protocols)
	protocols.SetUnencryptedHTTP2(true)
	srv := &http.Server{Handler: grpcHandler(p), Protocols: protocols, ReadHeaderTimeout: 10 * time.Second}
	go func() {
		if err := srv.Serve(l); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Printf("roll-csi-provider: %v", err)
		}
	}()
	return srv
}
