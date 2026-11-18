using Microsoft.Extensions.Configuration;

namespace Roll.Secrets.Configuration;

/// <summary>
/// roll secrets as an <see cref="IConfiguration"/> source: an environment's values (or a consumer's
/// resolve), with <c>__</c> in a key read as the section separator (<c>DB__HOST</c> is
/// <c>DB:HOST</c>), as the environment-variables source does.
/// </summary>
public sealed class RollSecretsConfigurationSource : IConfigurationSource
{
    /// <summary>The environment to read (<c>production</c>).</summary>
    public required string Environment { get; set; }

    /// <summary>The folder (<c>""</c>, the default, for the root).</summary>
    public string Path { get; set; } = "";

    /// <summary>Resolve as this consumer (<c>app:web</c>) instead of reading.</summary>
    public string? Consumer { get; set; }

    /// <summary>The client's options; null: all from the environment (<c>ROLL_API_URL</c>, <c>ROLL_TOKEN</c>…).</summary>
    public RollSecretsOptions? Client { get; set; }

    /// <summary>A failed first load leaves the source empty instead of throwing.</summary>
    public bool Optional { get; set; }

    /// <summary>Follow the environment (a long-polled watch) and reload the configuration when it changes.</summary>
    public bool ReloadOnChange { get; set; }

    /// <summary>The watch's pace when the server answers at once, and after a failure (default 5 s).</summary>
    public TimeSpan Every { get; set; } = TimeSpan.FromSeconds(5);

    /// <summary>A watch's failed check or fetch; the watch goes on.</summary>
    public Action<Exception>? OnError { get; set; }

    /// <inheritdoc />
    public IConfigurationProvider Build(IConfigurationBuilder builder) => new RollSecretsConfigurationProvider(this);
}

/// <summary>Reads a <see cref="RollSecretsConfigurationSource"/>, and follows it when it reloads on change.</summary>
public sealed class RollSecretsConfigurationProvider : ConfigurationProvider, IDisposable
{
    private readonly RollSecretsConfigurationSource _source;
    private readonly object _lock = new();
    private RollSecretsClient? _client;
    private SecretsWatch? _watch;

    /// <summary>A provider for <paramref name="source"/>; nothing is read until <see cref="Load"/>.</summary>
    public RollSecretsConfigurationProvider(RollSecretsConfigurationSource source)
    {
        _source = source;
    }

    /// <summary>Reads the values (blocking: configuration loads are synchronous), then starts the watch if asked.</summary>
    public override void Load()
    {
        try
        {
            _client ??= new RollSecretsClient(_source.Client);
            var client = _client;
            var v = Task.Run(() => _source.Consumer is { } consumer
                ? client.ResolveAsync(_source.Environment, _source.Path, consumer)
                : client.ValuesAsync(_source.Environment, _source.Path)).GetAwaiter().GetResult();
            if (v.Problems.Count > 0)
            {
                var p = v.Problems[0];
                throw new RollSecretsException(409, "unresolved", $"{_source.Environment} doesn't resolve: {p.Key}: {p.Description}");
            }
            lock (_lock)
            {
                Data = Map(v);
            }
        }
        catch (RollSecretsException) when (_source.Optional)
        {
            // an optional source stays as it was
        }
        if (_source.ReloadOnChange && _watch is null && _client is not null)
        {
            _watch = _client.Watch(_source.Environment, new WatchOptions
            {
                Path = _source.Path,
                Consumer = _source.Consumer,
                Every = _source.Every,
                OnError = _source.OnError,
                OnChange = Changed,
            });
        }
    }

    private void Changed(SecretValues v)
    {
        var data = Map(v);
        lock (_lock)
        {
            // The watch's first answer is usually what Load read: no reload for that.
            if (data.Count == Data.Count && data.All(kv => Data.TryGetValue(kv.Key, out var old) && old == kv.Value))
            {
                return;
            }
            Data = data;
        }
        OnReload();
    }

    private static Dictionary<string, string?> Map(SecretValues v)
    {
        var data = new Dictionary<string, string?>(StringComparer.OrdinalIgnoreCase);
        foreach (var (key, value) in v.Values)
        {
            data[key.Replace("__", ConfigurationPath.KeyDelimiter)] = value;
        }
        return data;
    }

    /// <summary>Stops the watch and closes the client.</summary>
    public void Dispose()
    {
        _watch?.Dispose();
        _client?.Dispose();
    }

    /// <summary>Names the environment only.</summary>
    public override string ToString() => $"RollSecretsConfigurationProvider {{ {_source.Environment} }}";
}

/// <summary>Adds roll secrets to a configuration.</summary>
public static class RollSecretsConfigurationExtensions
{
    /// <summary>
    /// Adds <paramref name="environment"/>'s values as configuration: <c>__</c> in a key is the
    /// section separator. <paramref name="configure"/> sets the folder, a consumer, the client and
    /// <see cref="RollSecretsConfigurationSource.ReloadOnChange"/>.
    /// </summary>
    public static IConfigurationBuilder AddRollSecrets(this IConfigurationBuilder builder, string environment, Action<RollSecretsConfigurationSource>? configure = null)
    {
        ArgumentNullException.ThrowIfNull(builder);
        var source = new RollSecretsConfigurationSource { Environment = environment };
        configure?.Invoke(source);
        return builder.Add(source);
    }
}
