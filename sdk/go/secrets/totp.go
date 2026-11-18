package secrets

import (
	"context"
	"crypto/hmac"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/sha512"
	"encoding/base32"
	"encoding/binary"
	"errors"
	"fmt"
	"hash"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// TOTP is a TOTP key's one-time code (docs/secrets.md, "TOTP keys") and when it stops being
// valid. Code is a credential: don't log it.
type TOTP struct {
	Code string `json:"code"`
	// ExpiresAt is in milliseconds since the epoch: the end of the code's step.
	ExpiresAt int64 `json:"expires_at"`
	// Period is how many seconds each code is valid for.
	Period int `json:"period"`
}

// TOTP is the current code of env's TOTP key under path ("" for the root). It needs reveal, as
// reading the seed does, and is audited without the code.
func (c *Client) TOTP(ctx context.Context, env, path, key string) (*TOTP, error) {
	e, err := c.env(ctx, env)
	if err != nil {
		return nil, err
	}
	var t TOTP
	return &t, c.call(ctx, http.MethodGet, e+"/secrets/"+url.PathEscape(key)+"/totp"+query(path), nil, &t)
}

// TOTPAt is the code of seed (an otpauth://totp/ URI or bare base32: SHA1, 6 digits, 30 s) at t,
// as the server computes it (RFC 6238): for a seed a sealed resolve already gave. Errors never
// quote the seed.
func TOTPAt(seed string, t time.Time) (*TOTP, error) {
	secret, alg, digits, period, err := parseTOTP(seed)
	if err != nil {
		return nil, err
	}
	unix := uint64(t.Unix())
	mac := hmac.New(alg, secret)
	_ = binary.Write(mac, binary.BigEndian, unix/period)
	h := mac.Sum(nil)
	o := h[len(h)-1] & 0x0f
	n := binary.BigEndian.Uint32(h[o:o+4]) & 0x7fffffff
	mod := uint32(1)
	for range digits {
		mod *= 10
	}
	return &TOTP{Code: fmt.Sprintf("%0*d", digits, n%mod), ExpiresAt: int64((unix/period+1)*period) * 1000, Period: int(period)}, nil
}

func parseTOTP(seed string) (secret []byte, alg func() hash.Hash, digits int, period uint64, err error) {
	seed = strings.TrimSpace(seed)
	alg, digits, period = sha1.New, 6, 30
	raw := seed
	if len(seed) >= 10 && strings.EqualFold(seed[:10], "otpauth://") {
		if len(seed) < 15 || !strings.EqualFold(seed[:15], "otpauth://totp/") {
			return nil, nil, 0, 0, errors.New("an otpauth URI must be otpauth://totp/… (HOTP isn't supported)")
		}
		_, q, _ := strings.Cut(seed, "?")
		q, _, _ = strings.Cut(q, "#")
		vals, perr := url.ParseQuery(q)
		if perr != nil {
			return nil, nil, 0, 0, errors.New("an otpauth URI's query is badly escaped")
		}
		raw = vals.Get("secret")
		if raw == "" {
			return nil, nil, 0, 0, errors.New("an otpauth URI needs ?secret=")
		}
		if a := vals.Get("algorithm"); a != "" {
			switch strings.ToUpper(a) {
			case "SHA1":
			case "SHA256":
				alg = sha256.New
			case "SHA512":
				alg = sha512.New
			default:
				return nil, nil, 0, 0, errors.New("algorithm must be SHA1, SHA256 or SHA512")
			}
		}
		if d := vals.Get("digits"); d != "" {
			if digits, err = strconv.Atoi(d); err != nil || digits < 6 || digits > 8 {
				return nil, nil, 0, 0, errors.New("digits must be 6–8")
			}
		}
		if p := vals.Get("period"); p != "" {
			if period, err = strconv.ParseUint(p, 10, 64); err != nil || period < 15 || period > 120 {
				return nil, nil, 0, 0, errors.New("period must be 15–120 seconds")
			}
		}
	}
	raw = strings.ToUpper(strings.TrimRight(strings.ReplaceAll(raw, " ", ""), "="))
	secret, err = base32.StdEncoding.WithPadding(base32.NoPadding).DecodeString(raw)
	if err != nil {
		return nil, nil, 0, 0, errors.New("a TOTP seed is an otpauth://totp/ URI or base32")
	}
	if len(secret) < 10 || len(secret) > 128 {
		return nil, nil, 0, 0, errors.New("a TOTP secret is 16–205 base32 characters (80–1024 bits)")
	}
	return secret, alg, digits, period, nil
}
