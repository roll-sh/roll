// roll queues compatibility scenarios for MassTransit (RabbitMQ transport).
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Text;
using Contracts;
using MassTransit;
using RabbitMQ.Client;

namespace Contracts
{
    public record Ping(string Text, int N);

    public record Pong(string Text, int N);
}

static class Program
{
    static readonly Uri Url = new(Environment.GetEnvironmentVariable("AMQP_URL") ?? "amqp://guest:guest@localhost:5672/%2F");
    static readonly string P = "mt-" + Guid.NewGuid().ToString("N")[..6];

    static async Task<int> Main(string[] args)
    {
        var scenarios = new (string Name, Func<string, Task<string?>> Fn)[]
        {
            ("connect", Connect), ("publish_consume", PublishConsume), ("send_endpoint", SendEndpoint),
            ("request_response", RequestResponse), ("multiple_consumers", MultipleConsumers),
            ("retry_error_queue", RetryErrorQueue), ("fault_event", FaultEvent), ("throughput", Throughput),
        };
        var only = args.ToHashSet();
        var failed = 0;
        foreach (var (name, fn) in scenarios)
        {
            if (only.Count > 0 && !only.Contains(name)) continue;
            var sw = Stopwatch.StartNew();
            var prefix = $"{P}.{name}";
            try
            {
                var detail = await fn(prefix).WaitAsync(TimeSpan.FromSeconds(name == "throughput" ? 30 : 15));
                Console.WriteLine($"PASS {name} {(detail is null ? "" : detail + " ")}({sw.Elapsed.TotalSeconds:F1}s)");
            }
            catch (Exception e)
            {
                failed++;
                Console.WriteLine($"FAIL {name}: {e.GetType().Name}: {e.Message.ReplaceLineEndings(" ")}");
                Console.Error.WriteLine(e);
            }
            finally
            {
                await Cleanup(prefix);
            }
        }
        return failed > 0 ? 1 : 0;
    }

    // ---- helpers ----

    static readonly ConcurrentDictionary<string, List<string>> Created = new();

    // Builds a bus whose message-type exchanges are prefixed per scenario, so nothing is shared between scenarios/runs.
    static async Task<IBusControl> StartBus(string prefix, Action<IRabbitMqBusFactoryConfigurator> configure)
    {
        IEntityNameFormatter? fmt = null;
        var bus = Bus.Factory.CreateUsingRabbitMq(cfg =>
        {
            var user = Url.UserInfo.Split(':');
            cfg.Host(Url.Host, (ushort)Url.Port, "/", h =>
            {
                h.Username(Uri.UnescapeDataString(user[0]));
                h.Password(Uri.UnescapeDataString(user[1]));
            });
            cfg.MessageTopology.SetEntityNameFormatter(new PrefixEntityNameFormatter(cfg.MessageTopology.EntityNameFormatter, prefix + "."));
            fmt = cfg.MessageTopology.EntityNameFormatter;
            configure(cfg);
        });
        Created.GetOrAdd(prefix, _ => new List<string>()).AddRange(new[]
        {
            fmt!.FormatEntityName<Ping>(), fmt.FormatEntityName<Pong>(), fmt.FormatEntityName<Fault<Ping>>(),
            fmt.FormatEntityName<Fault>(),
        });
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        await bus.StartAsync(cts.Token);
        return bus;
    }

    static async Task WithBus(IBusControl bus, Func<Task> body)
    {
        try
        {
            await body();
        }
        finally
        {
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(5));
            await bus.StopAsync(cts.Token);
        }
    }

    static string Endpoint(string prefix, string name)
    {
        var e = $"{prefix}.{name}";
        Created.GetOrAdd(prefix, _ => new List<string>()).Add(e);
        return e;
    }

    static Task<IConnection> Raw() => new ConnectionFactory { Uri = Url, AutomaticRecoveryEnabled = false }.CreateConnectionAsync();

    // Deletes every queue/exchange a scenario created (endpoint queue + exchange, _error/_skipped, message exchanges).
    static async Task Cleanup(string prefix)
    {
        if (!Created.TryRemove(prefix, out var names)) return;
        try
        {
            await using var c = await Raw();
            foreach (var n in names.Distinct())
            {
                foreach (var (isQueue, entity) in new[] { (true, n), (true, n + "_error"), (true, n + "_skipped"), (false, n), (false, n + "_error"), (false, n + "_skipped") })
                {
                    try
                    {
                        await using var ch = await c.CreateChannelAsync();
                        if (isQueue) await ch.QueueDeleteAsync(entity);
                        else await ch.ExchangeDeleteAsync(entity);
                    }
                    catch (Exception e)
                    {
                        Console.Error.WriteLine($"cleanup {entity}: {e.Message}");
                    }
                }
            }
            await c.CloseAsync();
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"cleanup {prefix}: {e.Message}");
        }
    }

    static void Check(bool cond, string msg)
    {
        if (!cond) throw new Exception(msg);
    }

    static async Task<T> Within<T>(Task<T> t, string what, double seconds = 5)
    {
        try
        {
            return await t.WaitAsync(TimeSpan.FromSeconds(seconds));
        }
        catch (TimeoutException)
        {
            throw new Exception($"timed out waiting for {what}");
        }
    }

    static TaskCompletionSource<T> Tcs<T>() => new(TaskCreationOptions.RunContinuationsAsynchronously);

    // ---- scenarios ----

    static async Task<string?> Connect(string prefix)
    {
        var bus = await StartBus(prefix, cfg => cfg.ReceiveEndpoint(Endpoint(prefix, "q"), e => e.Handler<Ping>(_ => Task.CompletedTask)));
        BusHealthResult health = default;
        await WithBus(bus, async () =>
        {
            var end = DateTime.UtcNow.AddSeconds(10);
            while ((health = bus.CheckHealth()).Status != BusHealthStatus.Healthy && DateTime.UtcNow < end) await Task.Delay(100);
            Check(health.Status == BusHealthStatus.Healthy, $"bus health {health.Status}: {health.Description}");
        });
        return health.Description;
    }

    static async Task<string?> PublishConsume(string prefix)
    {
        var got = Tcs<Ping>();
        var bus = await StartBus(prefix, cfg => cfg.ReceiveEndpoint(Endpoint(prefix, "q"), e =>
            e.Handler<Ping>(ctx => { got.TrySetResult(ctx.Message); return Task.CompletedTask; })));
        await WithBus(bus, async () =>
        {
            await bus.Publish(new Ping("hello", 42));
            var m = await Within(got.Task, "published Ping");
            Check(m == new Ping("hello", 42), $"payload {m}");
        });
        return null;
    }

    static async Task<string?> SendEndpoint(string prefix)
    {
        var got = Tcs<Ping>();
        var q = Endpoint(prefix, "q");
        var bus = await StartBus(prefix, cfg => cfg.ReceiveEndpoint(q, e =>
        {
            e.ConfigureConsumeTopology = false; // no message-type binding: only a direct send can reach it
            e.Handler<Ping>(ctx => { got.TrySetResult(ctx.Message); return Task.CompletedTask; });
        }));
        await WithBus(bus, async () =>
        {
            var ep = await bus.GetSendEndpoint(new Uri($"queue:{q}"));
            await ep.Send(new Ping("direct", 7));
            var m = await Within(got.Task, "sent Ping");
            Check(m == new Ping("direct", 7), $"payload {m}");
        });
        return null;
    }

    static async Task<string?> RequestResponse(string prefix)
    {
        var q = Endpoint(prefix, "q");
        var bus = await StartBus(prefix, cfg => cfg.ReceiveEndpoint(q, e =>
            e.Handler<Ping>(ctx => ctx.RespondAsync(new Pong(ctx.Message.Text.ToUpperInvariant(), ctx.Message.N + 1)))));
        await WithBus(bus, async () =>
        {
            var client = bus.CreateRequestClient<Ping>(new Uri($"queue:{q}"), RequestTimeout.After(s: 10));
            var resp = await client.GetResponse<Pong>(new Ping("ping", 1));
            Check(resp.Message == new Pong("PING", 2), $"response {resp.Message}");
        });
        return null;
    }

    static async Task<string?> MultipleConsumers(string prefix)
    {
        TaskCompletionSource<Ping> a = Tcs<Ping>(), b = Tcs<Ping>();
        var bus = await StartBus(prefix, cfg =>
        {
            cfg.ReceiveEndpoint(Endpoint(prefix, "a"), e => e.Handler<Ping>(ctx => { a.TrySetResult(ctx.Message); return Task.CompletedTask; }));
            cfg.ReceiveEndpoint(Endpoint(prefix, "b"), e => e.Handler<Ping>(ctx => { b.TrySetResult(ctx.Message); return Task.CompletedTask; }));
        });
        await WithBus(bus, async () =>
        {
            await bus.Publish(new Ping("fanout", 3));
            var ma = await Within(a.Task, "copy on endpoint a");
            var mb = await Within(b.Task, "copy on endpoint b");
            Check(ma == new Ping("fanout", 3) && mb == ma, $"payloads {ma} {mb}");
        });
        return null;
    }

    static async Task<string?> RetryErrorQueue(string prefix)
    {
        var attempts = 0;
        var q = Endpoint(prefix, "q");
        var bus = await StartBus(prefix, cfg => cfg.ReceiveEndpoint(q, e =>
        {
            e.UseMessageRetry(r => r.Immediate(2));
            e.Handler<Ping>(_ =>
            {
                Interlocked.Increment(ref attempts);
                throw new InvalidOperationException("boom");
            });
        }));
        string? detail = null;
        await WithBus(bus, async () =>
        {
            await bus.Publish(new Ping("poison", 9));
            await using var c = await Raw();
            var ch = await c.CreateChannelAsync();
            BasicGetResult? m = null;
            var end = DateTime.UtcNow.AddSeconds(10);
            while (m is null && DateTime.UtcNow < end)
            {
                try
                {
                    m = await ch.BasicGetAsync($"{q}_error", autoAck: true);
                }
                catch (RabbitMQ.Client.Exceptions.OperationInterruptedException e) when (e.ShutdownReason?.ReplyCode == 404)
                {
                    ch = await c.CreateChannelAsync(); // MassTransit declares the _error queue lazily on the first fault
                }
                if (m is null) await Task.Delay(100);
            }
            Check(m is not null, $"no message on {q}_error");
            var h = m!.BasicProperties.Headers ?? new Dictionary<string, object?>();
            string H(string k) => h.TryGetValue(k, out var v) ? v is byte[] bs ? Encoding.UTF8.GetString(bs) : $"{v}" : "";
            var body = Encoding.UTF8.GetString(m.Body.Span);
            Check(body.Contains("\"poison\""), $"error queue body {body}");
            Check(H("MT-Fault-Message") == "boom" && H("MT-Reason") == "fault", $"fault headers reason={H("MT-Reason")} message={H("MT-Fault-Message")}");
            Check(attempts == 3, $"handler ran {attempts} times, expected 3 (1 + 2 retries)");
            detail = $"3 attempts, retry-count header {H("MT-Fault-RetryCount")}";
        });
        return detail;
    }

    static async Task<string?> FaultEvent(string prefix)
    {
        var fault = Tcs<Fault<Ping>>();
        var bus = await StartBus(prefix, cfg =>
        {
            cfg.ReceiveEndpoint(Endpoint(prefix, "work"), e => e.Handler<Ping>(_ => throw new InvalidOperationException("kaput")));
            cfg.ReceiveEndpoint(Endpoint(prefix, "faults"), e => e.Handler<Fault<Ping>>(ctx => { fault.TrySetResult(ctx.Message); return Task.CompletedTask; }));
        });
        await WithBus(bus, async () =>
        {
            await bus.Publish(new Ping("doomed", 5));
            var f = await Within(fault.Task, "Fault<Ping>");
            Check(f.Message == new Ping("doomed", 5), $"fault message {f.Message}");
            Check(f.Exceptions.Length > 0 && f.Exceptions[0].Message == "kaput", $"fault exceptions {string.Join(",", f.Exceptions.Select(x => x.Message))}");
        });
        return null;
    }

    static async Task<string?> Throughput(string prefix)
    {
        const int n = 10_000;
        var got = 0;
        var done = Tcs<bool>();
        var bus = await StartBus(prefix, cfg => cfg.ReceiveEndpoint(Endpoint(prefix, "q"), e =>
        {
            e.PrefetchCount = 500;
            e.Handler<Ping>(_ =>
            {
                if (Interlocked.Increment(ref got) == n) done.TrySetResult(true);
                return Task.CompletedTask;
            });
        }));
        string? detail = null;
        await WithBus(bus, async () =>
        {
            var text = new string('x', 64);
            var sw = Stopwatch.StartNew();
            for (var i = 0; i < n; i += 500) // 500 publishes in flight, each awaiting its publisher confirm
                await Task.WhenAll(Enumerable.Range(i, 500).Select(j => bus.Publish(new Ping(text, j))));
            var pub = sw.Elapsed.TotalSeconds;
            if (await Task.WhenAny(done.Task, Task.Delay(20_000)) != done.Task)
                throw new Exception($"timed out after {Volatile.Read(ref got)} of {n} messages");
            var total = sw.Elapsed.TotalSeconds;
            detail = $"publish {n / pub:F0} msg/s, end-to-end {n / total:F0} msg/s";
        });
        return detail;
    }
}
