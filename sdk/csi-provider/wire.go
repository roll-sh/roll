package main

// The Secrets Store CSI Driver's provider protocol (provider/v1alpha1/service.proto in
// kubernetes-sigs/secrets-store-csi-driver): gRPC on a unix socket, two calls and seven small
// messages. gRPC is HTTP/2 with framed protobuf messages and the status in trailers; Go's own
// net/http serves unencrypted HTTP/2 (Go 1.24), and the messages are encoded here, so the
// provider needs the standard library only. A field this doesn't know is skipped, as protobuf
// does.

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
)

type versionRequest struct{ Version string }

type versionResponse struct{ Version, RuntimeName, RuntimeVersion string }

type mountRequest struct {
	Attributes, Secrets, TargetPath, Permission string
	CurrentObjectVersion                        []objectVersion
}

type mountResponse struct {
	ObjectVersion []objectVersion
	Error         *mountError
	Files         []file
}

type objectVersion struct{ ID, Version string }

type mountError struct{ Code string }

type file struct {
	Path     string
	Mode     int32
	Contents []byte
}

// Protobuf's wire format, as far as these messages use it.
const (
	varintType = 0
	fixed64    = 1
	bytesType  = 2
	fixed32    = 5
)

func appendVarint(b []byte, v uint64) []byte { return binary.AppendUvarint(b, v) }

func appendBytes(b []byte, num int, v []byte) []byte {
	b = appendVarint(b, uint64(num)<<3|bytesType)
	b = appendVarint(b, uint64(len(v)))
	return append(b, v...)
}

func appendString(b []byte, num int, s string) []byte {
	if s == "" {
		return b
	}
	return appendBytes(b, num, []byte(s))
}

var errMalformed = errors.New("a malformed protobuf message")

// fields calls each for every field of b: its number and wire type, and the value (the bytes of a
// length-delimited one, or a varint). Fixed-width fields are skipped.
func fields(b []byte, each func(num int, typ int, data []byte, v uint64)) error {
	for len(b) > 0 {
		tag, n := binary.Uvarint(b)
		if n <= 0 || tag>>3 == 0 || tag>>3 > 1<<29 {
			return errMalformed
		}
		b = b[n:]
		num, typ := int(tag>>3), int(tag&7)
		switch typ {
		case varintType:
			v, n := binary.Uvarint(b)
			if n <= 0 {
				return errMalformed
			}
			each(num, typ, nil, v)
			b = b[n:]
		case bytesType:
			l, n := binary.Uvarint(b)
			if n <= 0 || l > uint64(len(b)-n) {
				return errMalformed
			}
			each(num, typ, b[n:n+int(l)], 0)
			b = b[n+int(l):]
		case fixed64, fixed32:
			w := 8
			if typ == fixed32 {
				w = 4
			}
			if len(b) < w {
				return errMalformed
			}
			b = b[w:]
		default:
			return errMalformed
		}
	}
	return nil
}

func (m *versionRequest) marshal() []byte { return appendString(nil, 1, m.Version) }

func (m *versionRequest) unmarshal(b []byte) error {
	return fields(b, func(num, typ int, data []byte, _ uint64) {
		if num == 1 && typ == bytesType {
			m.Version = string(data)
		}
	})
}

func (m *versionResponse) marshal() []byte {
	b := appendString(nil, 1, m.Version)
	b = appendString(b, 2, m.RuntimeName)
	return appendString(b, 3, m.RuntimeVersion)
}

func (m *versionResponse) unmarshal(b []byte) error {
	return fields(b, func(num, typ int, data []byte, _ uint64) {
		if typ != bytesType {
			return
		}
		switch num {
		case 1:
			m.Version = string(data)
		case 2:
			m.RuntimeName = string(data)
		case 3:
			m.RuntimeVersion = string(data)
		}
	})
}

func (m *objectVersion) marshal() []byte {
	return appendString(appendString(nil, 1, m.ID), 2, m.Version)
}

func (m *objectVersion) unmarshal(b []byte) error {
	return fields(b, func(num, typ int, data []byte, _ uint64) {
		if typ != bytesType {
			return
		}
		switch num {
		case 1:
			m.ID = string(data)
		case 2:
			m.Version = string(data)
		}
	})
}

func (m *mountRequest) marshal() []byte {
	b := appendString(nil, 1, m.Attributes)
	b = appendString(b, 2, m.Secrets)
	b = appendString(b, 3, m.TargetPath)
	b = appendString(b, 4, m.Permission)
	for i := range m.CurrentObjectVersion {
		b = appendBytes(b, 5, m.CurrentObjectVersion[i].marshal())
	}
	return b
}

func (m *mountRequest) unmarshal(b []byte) error {
	var inner error
	err := fields(b, func(num, typ int, data []byte, _ uint64) {
		if typ != bytesType {
			return
		}
		switch num {
		case 1:
			m.Attributes = string(data)
		case 2:
			m.Secrets = string(data)
		case 3:
			m.TargetPath = string(data)
		case 4:
			m.Permission = string(data)
		case 5:
			var v objectVersion
			inner = errors.Join(inner, v.unmarshal(data))
			m.CurrentObjectVersion = append(m.CurrentObjectVersion, v)
		}
	})
	return errors.Join(err, inner)
}

func (m *mountResponse) marshal() []byte {
	var b []byte
	for i := range m.ObjectVersion {
		b = appendBytes(b, 1, m.ObjectVersion[i].marshal())
	}
	if m.Error != nil {
		b = appendBytes(b, 2, appendString(nil, 1, m.Error.Code))
	}
	for _, f := range m.Files {
		fb := appendString(nil, 1, f.Path)
		if f.Mode != 0 {
			fb = appendVarint(appendVarint(fb, 2<<3|varintType), uint64(int64(f.Mode)))
		}
		if len(f.Contents) > 0 {
			fb = appendBytes(fb, 3, f.Contents)
		}
		b = appendBytes(b, 3, fb)
	}
	return b
}

func (m *mountResponse) unmarshal(b []byte) error {
	var inner error
	err := fields(b, func(num, typ int, data []byte, _ uint64) {
		if typ != bytesType {
			return
		}
		switch num {
		case 1:
			var v objectVersion
			inner = errors.Join(inner, v.unmarshal(data))
			m.ObjectVersion = append(m.ObjectVersion, v)
		case 2:
			m.Error = &mountError{}
			inner = errors.Join(inner, fields(data, func(num, typ int, data []byte, _ uint64) {
				if num == 1 && typ == bytesType {
					m.Error.Code = string(data)
				}
			}))
		case 3:
			var f file
			inner = errors.Join(inner, fields(data, func(num, typ int, data []byte, v uint64) {
				switch {
				case num == 1 && typ == bytesType:
					f.Path = string(data)
				case num == 2 && typ == varintType:
					f.Mode = int32(v)
				case num == 3 && typ == bytesType:
					f.Contents = append([]byte(nil), data...)
				}
			}))
			m.Files = append(m.Files, f)
		}
	})
	return errors.Join(err, inner)
}

// providerServer is the CSIDriverProvider service.
type providerServer interface {
	Version(context.Context, *versionRequest) (*versionResponse, error)
	Mount(context.Context, *mountRequest) (*mountResponse, error)
}

// gRPC status codes this answers with.
const (
	codeOK            = 0
	codeUnknown       = 2
	codeInvalid       = 3
	codeUnimplemented = 12
)

// maxMessage bounds a request: the driver's are a few kilobytes (attributes and tokens).
const maxMessage = 4 << 20

// grpcHandler serves the CSIDriverProvider service as gRPC does: POST /<service>/<method>, one
// length-prefixed message each way, and the status in the trailers.
func grpcHandler(s providerServer) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || !strings.HasPrefix(r.Header.Get("Content-Type"), "application/grpc") {
			http.Error(w, "gRPC only", http.StatusUnsupportedMediaType)
			return
		}
		w.Header().Set("Content-Type", "application/grpc")
		w.Header().Set("Trailer", "Grpc-Status, Grpc-Message")
		w.WriteHeader(http.StatusOK)
		reply, code, err := call(s, r)
		if err == nil {
			frame := make([]byte, 5, 5+len(reply))
			binary.BigEndian.PutUint32(frame[1:], uint32(len(reply)))
			if _, err = w.Write(append(frame, reply...)); err != nil {
				return
			}
		}
		w.Header().Set("Grpc-Status", strconv.Itoa(code))
		if err != nil {
			w.Header().Set("Grpc-Message", percentEncode(err.Error()))
		}
	})
}

// call reads the request's one message and answers it: the reply, or a status code and error.
func call(s providerServer, r *http.Request) ([]byte, int, error) {
	var head [5]byte
	if _, err := io.ReadFull(r.Body, head[:]); err != nil {
		return nil, codeInvalid, errors.New("no request message")
	}
	n := binary.BigEndian.Uint32(head[1:])
	if head[0] != 0 {
		return nil, codeUnimplemented, errors.New("compressed messages aren't supported")
	}
	if n > maxMessage {
		return nil, codeInvalid, fmt.Errorf("a request of %d bytes", n)
	}
	msg := make([]byte, n)
	if _, err := io.ReadFull(r.Body, msg); err != nil {
		return nil, codeInvalid, errors.New("a truncated request message")
	}
	switch r.URL.Path {
	case "/v1alpha1.CSIDriverProvider/Version":
		var req versionRequest
		if err := req.unmarshal(msg); err != nil {
			return nil, codeInvalid, err
		}
		v, err := s.Version(r.Context(), &req)
		if err != nil {
			return nil, codeUnknown, err
		}
		return v.marshal(), codeOK, nil
	case "/v1alpha1.CSIDriverProvider/Mount":
		var req mountRequest
		if err := req.unmarshal(msg); err != nil {
			return nil, codeInvalid, err
		}
		v, err := s.Mount(r.Context(), &req)
		if err != nil {
			return nil, codeUnknown, err
		}
		return v.marshal(), codeOK, nil
	}
	return nil, codeUnimplemented, fmt.Errorf("no method %s", r.URL.Path)
}

// percentEncode is gRPC's encoding of grpc-message: printable ASCII but '%' as is.
func percentEncode(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c >= 0x20 && c <= 0x7e && c != '%' {
			b.WriteByte(c)
		} else {
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}
