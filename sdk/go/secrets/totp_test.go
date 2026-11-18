package secrets

import (
	"encoding/base32"
	"strings"
	"testing"
	"time"
)

// RFC 6238, Appendix B: each algorithm's own test seed, 8 digits, 30 s steps.
func TestTOTPMatchesRFC6238(t *testing.T) {
	seeds := map[string]string{"SHA1": "12345678901234567890", "SHA256": "12345678901234567890123456789012", "SHA512": strings.Repeat("1234567890", 6) + "1234"}
	vectors := []struct {
		at    int64
		codes map[string]string
	}{
		{59, map[string]string{"SHA1": "94287082", "SHA256": "46119246", "SHA512": "90693936"}},
		{1111111109, map[string]string{"SHA1": "07081804", "SHA256": "68084774", "SHA512": "25091201"}},
		{1234567890, map[string]string{"SHA1": "89005924", "SHA256": "91819424", "SHA512": "93441116"}},
		{20000000000, map[string]string{"SHA1": "65353130", "SHA256": "77737706", "SHA512": "47863826"}},
	}
	for alg, seed := range seeds {
		uri := "otpauth://totp/roll:test?secret=" + base32.StdEncoding.EncodeToString([]byte(seed)) + "&algorithm=" + alg + "&digits=8"
		for _, v := range vectors {
			got, err := TOTPAt(uri, time.Unix(v.at, 0))
			if err != nil || got.Code != v.codes[alg] {
				t.Fatalf("%s at %d: %+v %v", alg, v.at, got, err)
			}
		}
	}
	// Bare base32: SHA1, 6 digits, 30 s.
	got, err := TOTPAt("gezd gnbv gy3t qojq gezd gnbv gy3t qojq", time.Unix(59, 0))
	if err != nil || got.Code != "287082" || got.ExpiresAt != 60_000 || got.Period != 30 {
		t.Fatalf("%+v %v", got, err)
	}
	if _, err := TOTPAt("otpauth://totp/x?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=9", time.Now()); err == nil || strings.Contains(err.Error(), "GEZD") {
		t.Fatalf("%v", err)
	}
}
