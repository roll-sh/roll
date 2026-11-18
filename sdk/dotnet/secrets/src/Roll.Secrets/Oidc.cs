using System.Net.Http.Headers;

namespace Roll.Secrets;

/// <summary>Where a workload's OIDC token comes from when nothing else says.</summary>
internal static class Oidc
{
    public static async Task<string> TokenAsync(HttpClient http, string api, Func<string, string?> env, CancellationToken ct)
    {
        if (env("ROLL_OIDC_TOKEN") is { } token)
        {
            return token;
        }
        if (env("ROLL_OIDC_TOKEN_FILE") is { } file)
        {
            try
            {
                // Read again for each login: Kubernetes replaces a projected token on disk.
                return await File.ReadAllTextAsync(file, ct).ConfigureAwait(false);
            }
            catch (IOException)
            {
                throw new RollSecretsException(0, "io", "ROLL_OIDC_TOKEN_FILE can't be read");
            }
            catch (UnauthorizedAccessException)
            {
                throw new RollSecretsException(0, "io", "ROLL_OIDC_TOKEN_FILE can't be read");
            }
        }
        var url = env("ACTIONS_ID_TOKEN_REQUEST_URL");
        var bearer = env("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
        if (url is null || bearer is null)
        {
            throw new RollSecretsException(400, "config", "ROLL_IDENTITY is set but there's no OIDC token: set ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE, or run in GitHub Actions with `permissions: id-token: write`");
        }
        // The server's own URL by default: a token minted for one roll server isn't good at another.
        var audience = env("ROLL_OIDC_AUDIENCE") ?? api;
        using var req = new HttpRequestMessage(HttpMethod.Get, url + (url.Contains('?') ? "&" : "?") + "audience=" + Uri.EscapeDataString(audience));
        req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", bearer);
        using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        cts.CancelAfter(TimeSpan.FromSeconds(30));
        try
        {
            using var res = await http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, cts.Token).ConfigureAwait(false);
            if (!res.IsSuccessStatusCode)
            {
                throw new RollSecretsException(0, "oidc", "GitHub Actions didn't give its OIDC token (does the job have `permissions: id-token: write`?)");
            }
            using var s = await res.Content.ReadAsStreamAsync(cts.Token).ConfigureAwait(false);
            var buf = new byte[64 << 10];
            int len = 0, n;
            while (len < buf.Length && (n = await s.ReadAsync(buf.AsMemory(len), cts.Token).ConfigureAwait(false)) > 0)
            {
                len += n;
            }
            using var d = Json.Parse(buf.AsMemory(0, len), "GitHub Actions' OIDC answer has no value");
            return Json.Str(d.RootElement, "value") is { Length: > 0 } v
                ? v
                : throw new RollSecretsException(0, "oidc", "GitHub Actions' OIDC answer has no value");
        }
        catch (RollSecretsException e) when (e.Code == "invalid")
        {
            throw new RollSecretsException(0, "oidc", "GitHub Actions' OIDC answer has no value");
        }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            throw new RollSecretsException(0, "timeout", "GitHub Actions didn't answer for its OIDC token in time");
        }
        catch (HttpRequestException e)
        {
            throw new RollSecretsException(0, "io", "GitHub Actions didn't answer for its OIDC token: " + e.Message, e);
        }
    }
}
