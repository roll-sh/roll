package secrets

import (
	"bufio"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// An AWS identity's login (docs/secrets.md, "AWS IAM"): the machine's own AWS credentials sign an
// sts:GetCallerIdentity that roll sends to STS, as Vault's AWS auth (IAM method) does. The signed
// request goes to roll, never to AWS from here.

const (
	awsBody        = "Action=GetCallerIdentity&Version=2011-06-15"
	awsContentType = "application/x-www-form-urlencoded; charset=utf-8"
)

// AWSCredentials sign an AWS identity's login.
type AWSCredentials struct {
	AccessKeyID, SecretAccessKey, SessionToken string
}

func sha256Hex(s string) string {
	h := sha256.Sum256([]byte(s))
	return hex.EncodeToString(h[:])
}

func hmacSHA256(key []byte, msg string) []byte {
	m := hmac.New(sha256.New, key)
	m.Write([]byte(msg))
	return m.Sum(nil)
}

// sigV4 is the Authorization header of a Signature Version 4 request: every header in `headers`
// (lowercase names) is signed, and `query` is already canonical.
func sigV4(c AWSCredentials, region, service, method, path, query string, headers map[string]string, payload, amzDate string) string {
	names := make([]string, 0, len(headers))
	for k := range headers {
		names = append(names, k)
	}
	sort.Strings(names)
	var canon strings.Builder
	for _, k := range names {
		canon.WriteString(k + ":" + strings.TrimSpace(headers[k]) + "\n")
	}
	signed := strings.Join(names, ";")
	request := strings.Join([]string{method, path, query, canon.String(), signed, sha256Hex(payload)}, "\n")
	day := amzDate[:8]
	scope := day + "/" + region + "/" + service + "/aws4_request"
	toSign := "AWS4-HMAC-SHA256\n" + amzDate + "\n" + scope + "\n" + sha256Hex(request)
	k := hmacSHA256(hmacSHA256(hmacSHA256(hmacSHA256([]byte("AWS4"+c.SecretAccessKey), day), region), service), "aws4_request")
	return "AWS4-HMAC-SHA256 Credential=" + c.AccessKeyID + "/" + scope + ", SignedHeaders=" + signed + ", Signature=" + hex.EncodeToString(hmacSHA256(k, toSign))
}

// stsEndpoint is STS's host for a region and the region it signs for: the global endpoint without one.
func stsEndpoint(region string) (string, string) {
	switch {
	case region == "":
		return "sts.amazonaws.com", "us-east-1"
	case strings.HasPrefix(region, "cn-"):
		return "sts." + region + ".amazonaws.com.cn", region
	default:
		return "sts." + region + ".amazonaws.com", region
	}
}

// AWSSignedRequest is a signed GetCallerIdentity for roll to send on, naming serverID in
// X-Roll-Server-ID (signed, so it can't be replayed at another server).
func AWSSignedRequest(c AWSCredentials, region, serverID string, at time.Time) map[string]any {
	host, signingRegion := stsEndpoint(region)
	amzDate := at.UTC().Format("20060102T150405Z")
	headers := map[string]string{"content-type": awsContentType, "host": host, "x-amz-date": amzDate, "x-roll-server-id": serverID}
	if c.SessionToken != "" {
		headers["x-amz-security-token"] = c.SessionToken
	}
	auth := sigV4(c, signingRegion, "sts", "POST", "/", "", headers, awsBody, amzDate)
	out := map[string]string{"authorization": auth}
	for k, v := range headers {
		out[k] = v
	}
	return map[string]any{"method": "POST", "url": "https://" + host + "/", "headers": out, "body": awsBody}
}

func awsRegion(env func(string) string) string {
	if r := env("AWS_REGION"); r != "" {
		return r
	}
	if r := env("AWS_DEFAULT_REGION"); r != "" {
		return r
	}
	return profileValue(awsFile(env, "AWS_CONFIG_FILE", "config"), configSection(env), "region")
}

func profileName(env func(string) string) string {
	if p := env("AWS_PROFILE"); p != "" {
		return p
	}
	if p := env("AWS_DEFAULT_PROFILE"); p != "" {
		return p
	}
	return "default"
}

func configSection(env func(string) string) string {
	if p := profileName(env); p != "default" {
		return "profile " + p
	}
	return "default"
}

func awsFile(env func(string) string, v, name string) string {
	if f := env(v); f != "" {
		return f
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".aws", name)
}

// profileValue is key in [section] of an AWS shared file.
func profileValue(file, section, key string) string {
	f, err := os.Open(file)
	if err != nil {
		return ""
	}
	defer f.Close()
	inside := false
	s := bufio.NewScanner(f)
	for s.Scan() {
		line := strings.TrimSpace(s.Text())
		if strings.HasPrefix(line, "[") && strings.HasSuffix(line, "]") {
			inside = strings.TrimSpace(line[1:len(line)-1]) == section
		} else if k, v, ok := strings.Cut(line, "="); inside && ok && strings.TrimSpace(k) == key {
			return strings.TrimSpace(v)
		}
	}
	return ""
}

// The container endpoint's address may only be local, or HTTPS (as the AWS SDKs allow it).
func containerURLOK(u *url.URL) bool {
	if u.Scheme == "https" {
		return true
	}
	if u.Scheme != "http" {
		return false
	}
	h := u.Hostname()
	if h == "localhost" {
		return true
	}
	ip := net.ParseIP(h)
	return ip != nil && (ip.IsLoopback() || ip.Equal(net.ParseIP("169.254.170.2")) || ip.Equal(net.ParseIP("169.254.170.23")) || ip.Equal(net.ParseIP("fd00:ec2::23")))
}

func getJSON(ctx context.Context, hc *http.Client, req *http.Request, what string) (AWSCredentials, error) {
	res, err := hc.Do(req.WithContext(ctx))
	if err != nil {
		return AWSCredentials{}, fmt.Errorf("asking %s for credentials: %w", what, err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		return AWSCredentials{}, fmt.Errorf("%s answered %s", what, res.Status)
	}
	var v struct{ AccessKeyId, SecretAccessKey, Token string }
	if err := json.NewDecoder(io.LimitReader(res.Body, 64<<10)).Decode(&v); err != nil || v.AccessKeyId == "" || v.SecretAccessKey == "" {
		return AWSCredentials{}, fmt.Errorf("%s answered no credentials", what)
	}
	return AWSCredentials{v.AccessKeyId, v.SecretAccessKey, v.Token}, nil
}

// AWSCredentialsFromEnv finds the machine's AWS credentials as the AWS SDKs do: the environment, the
// shared files' profile (static keys), a web identity token (IRSA), the container endpoint (ECS task
// roles, EKS Pod Identity), then the instance's metadata service (IMDSv2).
func AWSCredentialsFromEnv(ctx context.Context) (AWSCredentials, error) {
	return awsCredentials(ctx, func(k string) string { return strings.TrimSpace(os.Getenv(k)) })
}

func awsCredentials(ctx context.Context, env func(string) string) (AWSCredentials, error) {
	if k, s := env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY"); k != "" && s != "" {
		return AWSCredentials{k, s, env("AWS_SESSION_TOKEN")}, nil
	}
	shared, cfg := awsFile(env, "AWS_SHARED_CREDENTIALS_FILE", "credentials"), awsFile(env, "AWS_CONFIG_FILE", "config")
	value := func(k string) string {
		if v := profileValue(shared, profileName(env), k); v != "" {
			return v
		}
		return profileValue(cfg, configSection(env), k)
	}
	if k, s := value("aws_access_key_id"), value("aws_secret_access_key"); k != "" && s != "" {
		return AWSCredentials{k, s, value("aws_session_token")}, nil
	}
	if env("AWS_PROFILE") != "" && env("AWS_WEB_IDENTITY_TOKEN_FILE") == "" {
		return AWSCredentials{}, fmt.Errorf("AWS profile %s has no access keys: roll reads a profile's static keys (for SSO, `eval \"$(aws configure export-credentials --format env)\"` first)", profileName(env))
	}
	hc := &http.Client{Timeout: 5 * time.Second}
	// IRSA: the pod's projected token for the role, exchanged with STS (unsigned).
	if file, role := env("AWS_WEB_IDENTITY_TOKEN_FILE"), env("AWS_ROLE_ARN"); file != "" && role != "" {
		token, err := os.ReadFile(file)
		if err != nil {
			return AWSCredentials{}, err
		}
		session := env("AWS_ROLE_SESSION_NAME")
		if session == "" {
			session = fmt.Sprintf("roll-%d", time.Now().Unix())
		}
		host, _ := stsEndpoint(awsRegion(env))
		form := url.Values{"Action": {"AssumeRoleWithWebIdentity"}, "Version": {"2011-06-15"}, "RoleArn": {role}, "RoleSessionName": {session}, "WebIdentityToken": {strings.TrimSpace(string(token))}}
		req, err := http.NewRequestWithContext(ctx, http.MethodPost, "https://"+host+"/", strings.NewReader(form.Encode()))
		if err != nil {
			return AWSCredentials{}, err
		}
		req.Header.Set("Content-Type", awsContentType)
		res, err := hc.Do(req)
		if err != nil {
			return AWSCredentials{}, fmt.Errorf("asking STS for the web identity's credentials: %w", err)
		}
		defer res.Body.Close()
		var v struct {
			Result struct {
				Credentials struct{ AccessKeyId, SecretAccessKey, SessionToken string }
			} `xml:"AssumeRoleWithWebIdentityResult"`
			Error struct{ Code string }
		}
		if err := xml.NewDecoder(io.LimitReader(res.Body, 64<<10)).Decode(&v); err != nil || res.StatusCode != http.StatusOK {
			return AWSCredentials{}, fmt.Errorf("STS refused the web identity (%s): %s", res.Status, v.Error.Code)
		}
		c := v.Result.Credentials
		return AWSCredentials{c.AccessKeyId, c.SecretAccessKey, c.SessionToken}, nil
	}
	// ECS task roles (relative) and EKS Pod Identity (full, with a token).
	endpoint := env("AWS_CONTAINER_CREDENTIALS_FULL_URI")
	if rel := env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"); rel != "" {
		endpoint = "http://169.254.170.2" + rel
	}
	if endpoint != "" {
		u, err := url.Parse(endpoint)
		if err != nil || !containerURLOK(u) {
			return AWSCredentials{}, fmt.Errorf("the container credentials endpoint is local or https, not %s", endpoint)
		}
		req, _ := http.NewRequest(http.MethodGet, u.String(), nil)
		auth := env("AWS_CONTAINER_AUTHORIZATION_TOKEN")
		if f := env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"); f != "" {
			b, err := os.ReadFile(f)
			if err != nil {
				return AWSCredentials{}, err
			}
			auth = strings.TrimSpace(string(b))
		}
		if auth != "" {
			req.Header.Set("Authorization", auth)
		}
		return getJSON(ctx, hc, req, "the container endpoint")
	}
	if strings.EqualFold(env("AWS_EC2_METADATA_DISABLED"), "true") {
		return AWSCredentials{}, errors.New("no AWS credentials (and the instance metadata service is off)")
	}
	c, err := imds(ctx, env)
	if err != nil {
		return AWSCredentials{}, fmt.Errorf("no AWS credentials in the environment, a profile, a web identity or a container, and the instance metadata service: %w", err)
	}
	return c, nil
}

// imds is IMDSv2: a session token, the instance profile's role, its credentials.
func imds(ctx context.Context, env func(string) string) (AWSCredentials, error) {
	base := strings.TrimRight(env("AWS_EC2_METADATA_SERVICE_ENDPOINT"), "/")
	if base == "" {
		base = "http://169.254.169.254"
	}
	hc := &http.Client{Timeout: 2 * time.Second}
	text := func(req *http.Request) (string, error) {
		res, err := hc.Do(req.WithContext(ctx))
		if err != nil {
			return "", err
		}
		defer res.Body.Close()
		if res.StatusCode != http.StatusOK {
			return "", fmt.Errorf("the instance metadata service answered %s", res.Status)
		}
		b, err := io.ReadAll(io.LimitReader(res.Body, 64<<10))
		return string(b), err
	}
	req, _ := http.NewRequest(http.MethodPut, base+"/latest/api/token", nil)
	req.Header.Set("X-aws-ec2-metadata-token-ttl-seconds", "300")
	token, err := text(req)
	if err != nil {
		return AWSCredentials{}, err
	}
	creds := base + "/latest/meta-data/iam/security-credentials/"
	req, _ = http.NewRequest(http.MethodGet, creds, nil)
	req.Header.Set("X-aws-ec2-metadata-token", token)
	roles, err := text(req)
	if err != nil {
		return AWSCredentials{}, err
	}
	role := strings.TrimSpace(strings.SplitN(roles, "\n", 2)[0])
	if role == "" {
		return AWSCredentials{}, errors.New("the instance has no instance profile")
	}
	req, _ = http.NewRequest(http.MethodGet, creds+url.PathEscape(role), nil)
	req.Header.Set("X-aws-ec2-metadata-token", token)
	return getJSON(ctx, hc, req, "the instance metadata service")
}
