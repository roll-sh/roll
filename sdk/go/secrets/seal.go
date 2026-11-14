package secrets

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/ecdh"
	"crypto/hkdf"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"errors"
)

// Sealed is a roll-seal-v1 box, as crates/roll-secrets/src/crypto.rs makes it: X25519 between a
// fresh key pair and the recipient's key, HKDF-SHA256 with salt epk ‖ recipient and info
// "roll-seal-v1" into an AES-256-GCM key, a random 12-byte nonce, and a context as the associated
// data, so a box opens only for the purpose it was sealed for.
type Sealed struct {
	V     int    `json:"v"`
	EPK   string `json:"epk"`
	Nonce string `json:"nonce"`
	CT    string `json:"ct"`
}

var b64 = base64.RawURLEncoding

// GenerateKey makes the X25519 key a sealed resolve is sealed to. Keep it in the process: its
// public key (PublicKey) is all the server needs.
func GenerateKey() (*ecdh.PrivateKey, error) {
	return ecdh.X25519().GenerateKey(rand.Reader)
}

// PublicKey is the base64url form of key's public key, what Resolve's sealing takes.
func PublicKey(key *ecdh.PrivateKey) string {
	return b64.EncodeToString(key.PublicKey().Bytes())
}

func boxKey(shared, epk, recipient []byte) ([]byte, error) {
	zero := true
	for _, b := range shared {
		zero = zero && b == 0
	}
	if zero {
		return nil, errors.New("not a usable X25519 key")
	}
	salt := append(append(make([]byte, 0, 64), epk...), recipient...)
	return hkdf.Key(sha256.New, shared, salt, "roll-seal-v1", 32)
}

// Open opens a box sealed to key for context.
func Open(box Sealed, key *ecdh.PrivateKey, context string) ([]byte, error) {
	fail := errors.New("the box doesn't open")
	if box.V != 1 {
		return nil, fail
	}
	epk, err1 := b64.DecodeString(box.EPK)
	nonce, err2 := b64.DecodeString(box.Nonce)
	ct, err3 := b64.DecodeString(box.CT)
	if err1 != nil || err2 != nil || err3 != nil || len(epk) != 32 || len(nonce) != 12 {
		return nil, fail
	}
	pub, err := ecdh.X25519().NewPublicKey(epk)
	if err != nil {
		return nil, fail
	}
	shared, err := key.ECDH(pub)
	if err != nil {
		return nil, fail
	}
	k, err := boxKey(shared, epk, key.PublicKey().Bytes())
	if err != nil {
		return nil, fail
	}
	block, _ := aes.NewCipher(k)
	gcm, _ := cipher.NewGCM(block)
	plain, err := gcm.Open(nil, nonce, ct, []byte(context))
	if err != nil {
		return nil, fail
	}
	return plain, nil
}
