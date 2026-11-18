package main

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/roll-sh/roll/sdk/go/secrets"
)

// connector is how the extension logs in, from the function's environment:
//   - ROLL_TOKEN: a roll token as it is (the function's configuration holds it);
//   - ROLL_IDENTITY with ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE: a workload identity's login with
//     that OIDC token, read again for each login;
//   - ROLL_IDENTITY alone: a workload identity's login with an OIDC token AWS STS signs for the
//     function's own IAM role (GetWebIdentityToken, IAM's outbound identity federation), so nothing
//     long-lived sits in the function's configuration.
//
// ROLL_PROJECT names the project when the token doesn't.
func connector(api string, env func(string) string, now func() time.Time) func(context.Context) (*secrets.Client, error) {
	get := func(k string) string { return strings.TrimSpace(env(k)) }
	return func(ctx context.Context) (*secrets.Client, error) {
		var c *secrets.Client
		switch identity := get("ROLL_IDENTITY"); {
		case get("ROLL_TOKEN") != "":
			c = secrets.New(api, get("ROLL_TOKEN"))
		case identity == "":
			return nil, errors.New("set ROLL_IDENTITY (a workload identity the function's IAM role logs in as) or ROLL_TOKEN")
		default:
			source := func(ctx context.Context) (string, error) {
				if t := get("ROLL_OIDC_TOKEN"); t != "" {
					return t, nil
				}
				if f := get("ROLL_OIDC_TOKEN_FILE"); f != "" {
					b, err := os.ReadFile(f)
					return string(b), err
				}
				audience := get("ROLL_OIDC_AUDIENCE")
				if audience == "" {
					audience = api
				}
				return webIdentityToken(ctx, env, audience, now())
			}
			var err error
			if c, err = secrets.LoginFunc(ctx, api, identity, source); err != nil {
				return nil, fmt.Errorf("logging in as %s: %w", identity, err)
			}
		}
		if p := get("ROLL_PROJECT"); p != "" {
			c.Project = p
		}
		return c, nil
	}
}

// webIdentityToken asks AWS STS for a JWT naming the function's IAM role (its `sub` is the role's
// ARN; `iss` the account's own issuer URL), signed RS256 (roll takes RS256 and ES256; STS signs
// RS256 and ES384) and valid for a minute: it's only exchanged for roll's login, which lasts the
// identity's TTL. The role needs `sts:GetWebIdentityToken`, and the account outbound identity
// federation turned on (`aws iam enable-outbound-web-identity-federation`).
func webIdentityToken(ctx context.Context, env func(string) string, audience string, now time.Time) (string, error) {
	creds := awsCredentials{id: env("AWS_ACCESS_KEY_ID"), secret: env("AWS_SECRET_ACCESS_KEY"), session: env("AWS_SESSION_TOKEN")}
	region := env("AWS_REGION")
	if creds.id == "" || creds.secret == "" || region == "" {
		return "", errors.New("ROLL_IDENTITY is set without ROLL_OIDC_TOKEN(_FILE), and there are no AWS credentials to ask STS for one (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION)")
	}
	// GetWebIdentityToken isn't on STS's global endpoint: the region's.
	endpoint := env("AWS_ENDPOINT_URL_STS")
	if endpoint == "" {
		endpoint = env("AWS_ENDPOINT_URL")
	}
	if endpoint == "" {
		endpoint = "https://sts." + region + ".amazonaws.com"
		if strings.HasPrefix(region, "cn-") {
			endpoint += ".cn"
		}
	}
	form := url.Values{
		"Action":            {"GetWebIdentityToken"},
		"Version":           {"2011-06-15"},
		"Audience.member.1": {audience},
		"SigningAlgorithm":  {"RS256"},
		"DurationSeconds":   {"60"},
	}
	body := []byte(form.Encode())
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, strings.TrimRight(endpoint, "/")+"/", bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")
	signV4(req, body, creds, region, "sts", now)
	res, err := (&http.Client{Timeout: 10 * time.Second}).Do(req)
	if err != nil {
		return "", fmt.Errorf("asking STS for a web identity token: %w", err)
	}
	defer res.Body.Close()
	data, err := io.ReadAll(io.LimitReader(res.Body, 64<<10))
	if err != nil {
		return "", err
	}
	if res.StatusCode != http.StatusOK {
		var e struct {
			Code    string `xml:"Error>Code"`
			Message string `xml:"Error>Message"`
		}
		_ = xml.Unmarshal(data, &e)
		hint := ""
		switch e.Code {
		case "OutboundWebIdentityFederationDisabled":
			hint = " (turn it on: aws iam enable-outbound-web-identity-federation)"
		case "AccessDenied":
			hint = " (the function's role needs sts:GetWebIdentityToken for audience " + audience + ")"
		}
		return "", fmt.Errorf("STS refused GetWebIdentityToken: %s %s: %s%s", res.Status, e.Code, e.Message, hint)
	}
	var ok struct {
		Token string `xml:"GetWebIdentityTokenResult>WebIdentityToken"`
	}
	if err := xml.Unmarshal(data, &ok); err != nil || ok.Token == "" {
		return "", errors.New("STS's GetWebIdentityToken answer has no WebIdentityToken")
	}
	return ok.Token, nil
}

type awsCredentials struct{ id, secret, session string }

// signV4 signs req, whose body is `body`, with AWS Signature Version 4 (every header set so far is
// signed).
func signV4(req *http.Request, body []byte, c awsCredentials, region, service string, now time.Time) {
	t := now.UTC()
	stamp, day := t.Format("20060102T150405Z"), t.Format("20060102")
	req.Header.Set("X-Amz-Date", stamp)
	if c.session != "" {
		req.Header.Set("X-Amz-Security-Token", c.session)
	}
	headers := map[string]string{"host": req.URL.Host}
	for k, v := range req.Header {
		for i := range v {
			v[i] = strings.Join(strings.Fields(v[i]), " ")
		}
		headers[strings.ToLower(k)] = strings.Join(v, ",")
	}
	names := make([]string, 0, len(headers))
	for k := range headers {
		names = append(names, k)
	}
	sort.Strings(names)
	var canonHeaders strings.Builder
	for _, k := range names {
		canonHeaders.WriteString(k + ":" + headers[k] + "\n")
	}
	signed := strings.Join(names, ";")
	path := req.URL.EscapedPath()
	if path == "" {
		path = "/"
	}
	q := req.URL.Query()
	keys := make([]string, 0, len(q))
	for k := range q {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var query []string
	for _, k := range keys {
		vs := append([]string(nil), q[k]...)
		sort.Strings(vs)
		for _, v := range vs {
			query = append(query, awsEscape(k)+"="+awsEscape(v))
		}
	}
	canonical := strings.Join([]string{req.Method, path, strings.Join(query, "&"), canonHeaders.String(), signed, hexSHA256(body)}, "\n")
	scope := day + "/" + region + "/" + service + "/aws4_request"
	toSign := "AWS4-HMAC-SHA256\n" + stamp + "\n" + scope + "\n" + hexSHA256([]byte(canonical))
	key := []byte("AWS4" + c.secret)
	for _, part := range []string{day, region, service, "aws4_request"} {
		key = hmacSHA256(key, part)
	}
	req.Header.Set("Authorization", "AWS4-HMAC-SHA256 Credential="+c.id+"/"+scope+", SignedHeaders="+signed+", Signature="+hex.EncodeToString(hmacSHA256(key, toSign)))
}

// awsEscape is RFC 3986 percent-encoding of everything but the unreserved characters.
func awsEscape(s string) string {
	var b strings.Builder
	for _, c := range []byte(s) {
		if 'A' <= c && c <= 'Z' || 'a' <= c && c <= 'z' || '0' <= c && c <= '9' || c == '-' || c == '_' || c == '.' || c == '~' {
			b.WriteByte(c)
		} else {
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}

func hexSHA256(b []byte) string {
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

func hmacSHA256(key []byte, data string) []byte {
	m := hmac.New(sha256.New, key)
	m.Write([]byte(data))
	return m.Sum(nil)
}
