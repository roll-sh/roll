package secrets

import (
	"context"
	"encoding/base64"
	"net/http"
	"net/url"
)

// Transit is one of the project's encryption keys (docs/secrets.md, "Encryption keys"): data is
// encrypted, signed and HMACed by roll, and the key never leaves it. It mirrors @roll/secrets'
// transit(name).
type Transit struct {
	c    *Client
	name string
}

// TransitPublicKey is one version of an Ed25519 key's public half, as base64, PEM and a JWK.
type TransitPublicKey struct {
	Version   int               `json:"version"`
	PublicKey string            `json:"public_key"`
	PEM       string            `json:"pem"`
	JWK       map[string]string `json:"jwk"`
}

// Transit is the encryption key name.
func (c *Client) Transit(name string) *Transit { return &Transit{c: c, name: name} }

func (t *Transit) at(ctx context.Context, op string) (string, error) {
	p, err := t.c.ProjectID(ctx)
	if err != nil {
		return "", err
	}
	return "/v1/secrets/projects/" + url.PathEscape(p) + "/transit/" + url.PathEscape(t.name) + "/" + op, nil
}

func (t *Transit) post(ctx context.Context, op string, body, out any) error {
	path, err := t.at(ctx, op)
	if err != nil {
		return err
	}
	return t.c.call(ctx, http.MethodPost, path, body, out)
}

// Encrypt is data's ciphertext (`roll:v<n>:…`); encryptionContext, when given, must be given to
// decrypt it.
func (t *Transit) Encrypt(ctx context.Context, data []byte, encryptionContext string) (string, error) {
	var out struct {
		Ciphertext string `json:"ciphertext"`
	}
	err := t.post(ctx, "encrypt", map[string]any{"plaintext": base64.StdEncoding.EncodeToString(data), "context": optional(encryptionContext)}, &out)
	return out.Ciphertext, err
}

// Decrypt is the plaintext of ciphertext (audited, as a reveal).
func (t *Transit) Decrypt(ctx context.Context, ciphertext, encryptionContext string) ([]byte, error) {
	var out struct {
		Plaintext string `json:"plaintext"`
	}
	if err := t.post(ctx, "decrypt", map[string]any{"ciphertext": ciphertext, "context": optional(encryptionContext)}, &out); err != nil {
		return nil, err
	}
	return base64.StdEncoding.DecodeString(out.Plaintext)
}

// Sign is an Ed25519 signature over data as given: anyone with PublicKeys checks it.
func (t *Transit) Sign(ctx context.Context, data []byte) (string, error) {
	var out struct {
		Signature string `json:"signature"`
	}
	err := t.post(ctx, "sign", map[string]any{"input": base64.StdEncoding.EncodeToString(data)}, &out)
	return out.Signature, err
}

// Verify is whether signature is the key's over data.
func (t *Transit) Verify(ctx context.Context, data []byte, signature string) (bool, error) {
	var out struct {
		Valid bool `json:"valid"`
	}
	err := t.post(ctx, "verify", map[string]any{"input": base64.StdEncoding.EncodeToString(data), "signature": signature}, &out)
	return out.Valid, err
}

// HMAC is HMAC-SHA256 of data, the same for the same input: a lookup column over encrypted fields.
func (t *Transit) HMAC(ctx context.Context, data []byte) (string, error) {
	var out struct {
		HMAC string `json:"hmac"`
	}
	err := t.post(ctx, "hmac", map[string]any{"input": base64.StdEncoding.EncodeToString(data)}, &out)
	return out.HMAC, err
}

// PublicKeys are an Ed25519 key's public halves, every version.
func (t *Transit) PublicKeys(ctx context.Context) ([]TransitPublicKey, error) {
	path, err := t.at(ctx, "public")
	if err != nil {
		return nil, err
	}
	var out []TransitPublicKey
	return out, t.c.call(ctx, http.MethodGet, path, nil, &out)
}

// optional is s, or nil (left out of the JSON body) when empty.
func optional(s string) any {
	if s == "" {
		return nil
	}
	return s
}
