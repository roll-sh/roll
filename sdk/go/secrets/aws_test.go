package secrets

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// AWS's published example (the IAM ListUsers request in "Examples of the complete Signature Version
// 4 signing process"), with its documented example keys.
func TestSigV4SignsAsAWSPublishedExample(t *testing.T) {
	c := AWSCredentials{AccessKeyID: "AKIDEXAMPLE", SecretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}
	headers := map[string]string{"content-type": awsContentType, "host": "iam.amazonaws.com", "x-amz-date": "20150830T123600Z"}
	got := sigV4(c, "us-east-1", "iam", "GET", "/", "Action=ListUsers&Version=2010-05-08", headers, "", "20150830T123600Z")
	want := "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
	if got != want {
		t.Fatalf("got %s", got)
	}
}

// A pod with EKS Pod Identity: credentials from the container endpoint (with its token file), a
// GetCallerIdentity signed with them that names the server, and roll's token back.
func TestAnAWSIdentityLogsInWithContainerCredentials(t *testing.T) {
	creds := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "pod-identity-token" {
			w.WriteHeader(http.StatusForbidden)
			return
		}
		json.NewEncoder(w).Encode(map[string]string{"AccessKeyId": "AKIAIOSFODNN7EXAMPLE", "SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "Token": "EXAMPLETOKEN"})
	}))
	defer creds.Close()
	var sent map[string]any
	roll := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		json.NewDecoder(r.Body).Decode(&sent)
		json.NewEncoder(w).Encode(map[string]any{"token": "rlt_test", "project": "prj_shop", "expires_at": 0})
	}))
	defer roll.Close()
	tokenFile := filepath.Join(t.TempDir(), "token")
	os.WriteFile(tokenFile, []byte("pod-identity-token\n"), 0o600)
	env := map[string]string{
		"ROLL_API_URL": roll.URL, "ROLL_IDENTITY": "idn_web", "AWS_REGION": "eu-west-1",
		"AWS_CONTAINER_CREDENTIALS_FULL_URI": creds.URL + "/v1/credentials", "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE": tokenFile,
		"AWS_SHARED_CREDENTIALS_FILE": filepath.Join(t.TempDir(), "none"), "AWS_CONFIG_FILE": filepath.Join(t.TempDir(), "none"),
	}
	for _, k := range []string{"ROLL_TOKEN", "ROLL_OIDC_TOKEN", "ROLL_OIDC_TOKEN_FILE", "ACTIONS_ID_TOKEN_REQUEST_URL", "ROLL_IDENTITY_KIND", "ROLL_AWS_SERVER_ID", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_PROFILE", "AWS_WEB_IDENTITY_TOKEN_FILE", "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "AWS_CONTAINER_AUTHORIZATION_TOKEN"} {
		env[k] = ""
	}
	for k, v := range env {
		t.Setenv(k, v)
	}
	c, err := FromEnv(context.Background())
	if err != nil || c.Token != "rlt_test" {
		t.Fatalf("%v %+v", err, c)
	}
	aws, _ := sent["aws"].(map[string]any)
	headers, _ := aws["headers"].(map[string]any)
	auth, _ := headers["authorization"].(string)
	if sent["identity"] != "idn_web" || aws["url"] != "https://sts.eu-west-1.amazonaws.com/" || aws["body"] != awsBody || headers["x-roll-server-id"] != roll.URL || headers["x-amz-security-token"] != "EXAMPLETOKEN" {
		t.Fatalf("sent %v", sent)
	}
	if !strings.Contains(auth, "Credential=AKIAIOSFODNN7EXAMPLE/") || !strings.Contains(auth, "/eu-west-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-roll-server-id, ") {
		t.Fatalf("authorization %s", auth)
	}
}
