namespace Roll.Secrets;

/// <summary>
/// How a <see cref="RollSecretsClient"/> is made. Anything left null comes from the environment,
/// as <c>roll run</c> reads it: <c>ROLL_API_URL</c>, then <c>ROLL_TOKEN</c>, or
/// <c>ROLL_IDENTITY</c> with the workload's OIDC token from <c>ROLL_OIDC_TOKEN</c>,
/// <c>ROLL_OIDC_TOKEN_FILE</c> (a Kubernetes projected token) or GitHub Actions (audience
/// <c>ROLL_OIDC_AUDIENCE</c>, default the API's URL); <c>ROLL_PROJECT</c> names the project when
/// the token doesn't.
/// </summary>
/// <remarks>A class, not a record: its <c>ToString</c> never prints the token.</remarks>
public sealed class RollSecretsOptions
{
    /// <summary>rolld's API, e.g. <c>https://api.roll.example</c> (no trailing <c>/v1</c>).</summary>
    public string? Api { get; set; }

    /// <summary>A roll token (<c>rlt_…</c>) with a rule on the project, or the operator token.</summary>
    public string? Token { get; set; }

    /// <summary>The project's id (<c>prj_…</c>). A token with a rule on one project knows it.</summary>
    public string? Project { get; set; }

    /// <summary>
    /// Log in as a workload identity (<c>idn_…</c>) with the workload's own OIDC token instead of a
    /// token: the client logs in again before the roll token ends, and when the server refuses it.
    /// Ignored when a token is set.
    /// </summary>
    public string? Identity { get; set; }

    /// <summary>
    /// Where the identity's OIDC token comes from, asked again for each login. Null: the
    /// environment (<c>ROLL_OIDC_TOKEN</c>, <c>ROLL_OIDC_TOKEN_FILE</c>, GitHub Actions).
    /// </summary>
    public Func<CancellationToken, Task<string>>? OidcToken { get; set; }

    /// <summary>
    /// The HTTP client requests go through (its own <c>Timeout</c> applies too: keep it over 55 s
    /// for long polls). Null: one of the client's own.
    /// </summary>
    public HttpClient? HttpClient { get; set; }

    /// <summary>Each request's timeout (default 30 s); a long poll gets its wait on top.</summary>
    public TimeSpan Timeout { get; set; } = TimeSpan.FromSeconds(30);

    /// <summary>
    /// Keep <see cref="RollSecretsClient.ValuesAsync"/> answers this long and answer from memory
    /// meanwhile; after that, ask again with the answer's digest and every key's stamp
    /// (<c>…/values/delta</c>): an unchanged folder costs what a 304 does, and a changed one only
    /// its changed keys. A folder holding dynamic credentials is never kept (they end with their
    /// lease), nor one that didn't resolve, nor a full read's answer without an ETag. Zero (the
    /// default): off.
    /// </summary>
    public TimeSpan CacheTtl { get; set; } = TimeSpan.Zero;

    /// <summary>The clock the cache's TTL is measured with (for tests).</summary>
    public TimeProvider TimeProvider { get; set; } = TimeProvider.System;
}
