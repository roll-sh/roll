using System.Diagnostics;

namespace Roll.Secrets;

/// <summary>What <see cref="RollSecretsClient.Watch(string, WatchOptions)"/> follows and how.</summary>
public sealed class WatchOptions
{
    /// <summary>
    /// Gets the values at the start and again each time they change. Values that don't resolve
    /// never reach it: what's running keeps the previous ones. An exception it throws goes to
    /// <see cref="OnError"/>.
    /// </summary>
    public required Action<SecretValues> OnChange { get; init; }

    /// <summary>The folder (<c>""</c>, the default, for the root).</summary>
    public string Path { get; init; } = "";

    /// <summary>Resolve as this consumer (<c>app:web</c>) instead of reading: dynamic leases are the consumer's.</summary>
    public string? Consumer { get; init; }

    /// <summary>
    /// How soon to ask again when the server answered at once (one without long polls), and after
    /// a failure: the default is 5 s. A long poll is asked again as soon as it answers.
    /// </summary>
    public TimeSpan Every { get; init; } = TimeSpan.FromSeconds(5);

    /// <summary>How often the latest values' dynamic leases are renewed (default 10 minutes).</summary>
    public TimeSpan RenewEvery { get; init; } = TimeSpan.FromMinutes(10);

    /// <summary>A check, a fetch, a renewal or an <see cref="OnChange"/> that failed; the watch goes on.</summary>
    public Action<Exception>? OnError { get; init; }
}

/// <summary>
/// A running <see cref="RollSecretsClient.Watch(string, WatchOptions)"/>: a background loop that
/// follows the digest. Disposing it stops it, abandoning a held long poll; nothing is called back
/// once <see cref="Dispose"/> or <see cref="DisposeAsync"/> returns.
/// </summary>
public sealed class SecretsWatch : IDisposable, IAsyncDisposable
{
    /// <summary>A long poll's wait, the server's most.</summary>
    private static readonly TimeSpan Wait = RollSecretsClient.MaxWait;

    private readonly RollSecretsClient _client;
    private readonly string _env;
    private readonly WatchOptions _o;
    private readonly CancellationTokenSource _stop = new();
    private readonly object _lock = new();
    private readonly Task _loop;
    private volatile bool _stopped;
    private int _callbackThread = -1;

    internal SecretsWatch(RollSecretsClient client, string env, WatchOptions o)
    {
        _client = client;
        _env = env;
        _o = o;
        _loop = Task.Run(RunAsync);
    }

    /// <summary>Whether the watch is still running.</summary>
    public bool Running => !_stopped && !_loop.IsCompleted;

    private void Fail(Exception e)
    {
        // A stop's cancellation surfaces as a failed request: it isn't one.
        if (_stopped || _o.OnError is null)
        {
            return;
        }
        try
        {
            _o.OnError(e);
        }
        catch
        {
            // the watch goes on
        }
    }

    private async Task RunAsync()
    {
        var ct = _stop.Token;
        string? last = null;
        IReadOnlyList<string> leases = Array.Empty<string>();
        long renewAt = Stopwatch.GetTimestamp() + Ticks(_o.RenewEvery);
        while (!_stopped)
        {
            TimeSpan next = _o.Every;
            try
            {
                long asked = Stopwatch.GetTimestamp();
                string now = await _client.DigestSinceAsync(_env, _o.Path, last, Wait, ct).ConfigureAwait(false);
                bool changed = now != last;
                // At once after a changed or held answer; `Every` after an unchanged one that came
                // straight back (a server without long polls).
                if (changed || Stopwatch.GetElapsedTime(asked) >= TimeSpan.FromSeconds(1))
                {
                    next = TimeSpan.Zero;
                }
                if (changed)
                {
                    var v = await _client.LoadAsync(_env, _o, ct).ConfigureAwait(false); // a failure is retried on the next round
                    string? before = last;
                    last = now;
                    if (v.Problems.Count > 0)
                    {
                        // Nothing running yet: try again next round. Otherwise the next change tries again.
                        if (before is null)
                        {
                            last = null;
                        }
                        var p = v.Problems[0];
                        throw new RollSecretsException(409, "unresolved", $"{_env} doesn't resolve: {p.Key}: {p.Description}");
                    }
                    leases = v.Leases;
                    lock (_lock)
                    {
                        if (_stopped)
                        {
                            return;
                        }
                        _callbackThread = Environment.CurrentManagedThreadId;
                        try
                        {
                            _o.OnChange(v);
                        }
                        finally
                        {
                            _callbackThread = -1;
                        }
                    }
                }
            }
            catch (OperationCanceledException) when (_stopped)
            {
                return;
            }
            catch (Exception e)
            {
                next = _o.Every; // never at once after a failure: that would spin
                Fail(e);
            }
            if (Stopwatch.GetTimestamp() - renewAt >= 0)
            {
                renewAt = Stopwatch.GetTimestamp() + Ticks(_o.RenewEvery);
                foreach (var l in leases)
                {
                    if (_stopped)
                    {
                        return;
                    }
                    try
                    {
                        await _client.RenewAsync(l, 0, ct).ConfigureAwait(false);
                    }
                    catch (OperationCanceledException) when (_stopped)
                    {
                        return;
                    }
                    catch (Exception e)
                    {
                        Fail(e);
                    }
                }
            }
            if (next > TimeSpan.Zero)
            {
                var untilRenew = Stopwatch.GetElapsedTime(Stopwatch.GetTimestamp(), renewAt);
                var pause = untilRenew < next ? untilRenew : next;
                try
                {
                    await Task.Delay(pause > TimeSpan.FromMilliseconds(1) ? pause : TimeSpan.FromMilliseconds(1), ct).ConfigureAwait(false);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
            }
        }
    }

    private static long Ticks(TimeSpan t) => (long)(t.TotalSeconds * Stopwatch.Frequency);

    /// <summary>Stops the watch: no callback runs once it returns (but from inside one, which ends).</summary>
    private bool Stop()
    {
        lock (_lock)
        {
            // From inside OnChange the lock is already ours: the loop ends once the callback returns.
            _stopped = true;
        }
        _stop.Cancel();
        return _callbackThread != Environment.CurrentManagedThreadId;
    }

    /// <summary>Stops the watch and waits (up to 5 s) for its loop to end.</summary>
    public void Dispose()
    {
        if (Stop())
        {
            try
            {
                _loop.Wait(TimeSpan.FromSeconds(5));
            }
            catch (AggregateException)
            {
                // it ended
            }
        }
    }

    /// <summary>Stops the watch and waits for its loop to end.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Stop())
        {
            try
            {
                await _loop.WaitAsync(TimeSpan.FromSeconds(5)).ConfigureAwait(false);
            }
            catch (TimeoutException)
            {
                // it's stopped: nothing more is called back
            }
        }
    }

    /// <summary>Names the environment only.</summary>
    public override string ToString() => $"SecretsWatch {{ {_env}{(_stopped ? ", stopped" : "")} }}";
}
