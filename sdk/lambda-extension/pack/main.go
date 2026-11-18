// Command pack writes a Lambda layer zip holding one extension: `go run ./pack -o layer.zip
// build/amd64/roll-secrets` puts the binary at extensions/roll-secrets, executable (Lambda runs
// every file in /opt/extensions), with a fixed timestamp so the same binary gives the same zip.
package main

import (
	"archive/zip"
	"flag"
	"io"
	"log"
	"os"
	"path/filepath"
	"time"
)

func main() {
	out := flag.String("o", "layer.zip", "the zip to write")
	flag.Parse()
	if flag.NArg() != 1 {
		log.Fatal("usage: pack -o layer.zip <the extension's binary>")
	}
	bin := flag.Arg(0)
	in, err := os.Open(bin)
	if err != nil {
		log.Fatal(err)
	}
	defer in.Close()
	f, err := os.Create(*out)
	if err != nil {
		log.Fatal(err)
	}
	z := zip.NewWriter(f)
	h := &zip.FileHeader{Name: "extensions/" + filepath.Base(bin), Method: zip.Deflate, Modified: time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)}
	h.SetMode(0o755)
	w, err := z.CreateHeader(h)
	if err == nil {
		_, err = io.Copy(w, in)
	}
	if err == nil {
		err = z.Close()
	}
	if err == nil {
		err = f.Close()
	}
	if err != nil {
		_ = os.Remove(*out)
		log.Fatal(err)
	}
}
