// roll queues compatibility scenarios for RabbitMQ.Client 7.x (async API).
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using RabbitMQ.Client;
using RabbitMQ.Client.Events;
using RabbitMQ.Client.Exceptions;

static class Program
{
    static readonly string Url = Environment.GetEnvironmentVariable("AMQP_URL") ?? "amqp://guest:guest@localhost:5672/%2F";
    static readonly string P = "dotnet-" + Guid.NewGuid().ToString("N")[..6];
    static readonly CreateChannelOptions Confirms = new(publisherConfirmationsEnabled: true, publisherConfirmationTrackingEnabled: true);

    static async Task<int> Main(string[] args)
    {
        var scenarios = new (string Name, Func<Task<string?>> Fn)[]
        {
            ("connect", Connect), ("declare", Declare), ("publish_confirms", PublishConfirms),
            ("consume_ack", ConsumeAck), ("nack_requeue", NackRequeue), ("reject_dlx", RejectDlx),
            ("topic", Topic), ("headers", Headers), ("tx", Tx), ("prefetch", Prefetch), ("ttl", Ttl),
            ("priority", Priority), ("exclusive_autodelete", ExclusiveAutodelete), ("direct_reply_to", DirectReplyTo),
            ("basic_get", BasicGet), ("cancel_notify", CancelNotify), ("connection_blocked", ConnectionBlocked),
            ("heartbeat", Heartbeat), ("large_message", LargeMessage), ("throughput", Throughput),
        };
        var only = args.ToHashSet();
        var failed = 0;
        foreach (var (name, fn) in scenarios)
        {
            if (only.Count > 0 && !only.Contains(name)) continue;
            var sw = Stopwatch.StartNew();
            try
            {
                var detail = await fn().WaitAsync(TimeSpan.FromSeconds(name == "heartbeat" ? 20 : 15));
                Console.WriteLine($"PASS {name} {(detail is null ? "" : detail + " ")}({sw.Elapsed.TotalSeconds:F1}s)");
            }
            catch (Exception e)
            {
                failed++;
                Console.WriteLine($"FAIL {name}: {e.GetType().Name}: {e.Message.ReplaceLineEndings(" ")}");
                Console.Error.WriteLine(e);
            }
        }
        return failed > 0 ? 1 : 0;
    }

    // ---- helpers ----

    static Task<IConnection> Open(TimeSpan? heartbeat = null)
    {
        var f = new ConnectionFactory { Uri = new Uri(Url), AutomaticRecoveryEnabled = false, ClientProvidedName = P };
        if (heartbeat is { } hb) f.RequestedHeartbeat = hb;
        return f.CreateConnectionAsync();
    }

    static void Check(bool cond, string msg)
    {
        if (!cond) throw new Exception(msg);
    }

    static async Task<uint> Count(IChannel ch, string q) => (await ch.QueueDeclarePassiveAsync(q)).MessageCount;

    static Task<QueueDeclareOk> Queue(IChannel ch, string q, bool durable = false, bool exclusive = false,
        bool autoDelete = false, IDictionary<string, object?>? args = null) =>
        ch.QueueDeclareAsync(q, durable, exclusive, autoDelete, args);

    static async Task<BasicGetResult> GetWait(IChannel ch, string q, double timeout = 5)
    {
        var end = DateTime.UtcNow.AddSeconds(timeout);
        while (DateTime.UtcNow < end)
        {
            var r = await ch.BasicGetAsync(q, autoAck: false);
            if (r is not null) return r;
            await Task.Delay(50);
        }
        throw new Exception($"no message on {q} within {timeout}s");
    }

    static async Task<string> ExpectClose(Func<Task> fn, int code)
    {
        try
        {
            await fn();
        }
        catch (OperationInterruptedException e) when (e.ShutdownReason is not null)
        {
            Check(e.ShutdownReason.ReplyCode == code, $"expected {code}, got {e.ShutdownReason.ReplyCode} {e.ShutdownReason.ReplyText}");
            return e.ShutdownReason.ReplyText;
        }
        throw new Exception($"expected channel error {code}");
    }

    static async Task<bool> IsReturned(Func<Task> publish)
    {
        try
        {
            await publish();
            return false;
        }
        catch (PublishException e) when (e.IsReturn)
        {
            return true;
        }
    }

    static async Task Until(Func<bool> cond, double timeout = 5)
    {
        var end = DateTime.UtcNow.AddSeconds(timeout);
        while (!cond() && DateTime.UtcNow < end) await Task.Delay(20);
    }

    static string Str(object? v) => v is byte[] b ? Encoding.UTF8.GetString(b) : v?.ToString() ?? "";

    static ReadOnlyMemory<byte> B(string s) => Encoding.UTF8.GetBytes(s);

    // ---- scenarios ----

    static async Task<string?> Connect()
    {
        await using var c = await Open();
        var props = c.ServerProperties!;
        Check(c.IsOpen, "connection not open");
        await using var ch = await c.CreateChannelAsync();
        Check(ch.IsOpen, "channel not open");
        var detail = $"{Str(props["product"])} {Str(props["version"])}";
        await c.CloseAsync();
        return detail;
    }

    static async Task<string?> Declare()
    {
        await using var c = await Open();
        var ch = await c.CreateChannelAsync();
        string x = $"{P}.declare.x", q = $"{P}.declare.q";
        await ch.ExchangeDeclareAsync(x, ExchangeType.Direct, durable: true);
        var ok = await Queue(ch, q, durable: true);
        Check(ok.QueueName == q && ok.MessageCount == 0, $"declare-ok {ok.QueueName} {ok.MessageCount}");
        await ch.QueueBindAsync(q, x, "k");
        await ch.ExchangeDeclarePassiveAsync(x);
        var srv = (await Queue(ch, "", exclusive: true)).QueueName;
        Check(srv.StartsWith("amq.gen-"), $"server-named queue {srv}");
        var text = await ExpectClose(() => Queue(ch, q, durable: false), 406);
        Check(text.Contains("inequivalent arg 'durable'"), text);
        ch = await c.CreateChannelAsync();
        await ExpectClose(() => ch.QueueDeclarePassiveAsync($"{P}.missing"), 404);
        ch = await c.CreateChannelAsync();
        await ch.QueueUnbindAsync(q, x, "k");
        await ch.QueueDeleteAsync(q);
        await ch.ExchangeDeleteAsync(x);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> PublishConfirms()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        var q = $"{P}.confirms";
        await Queue(ch, q, durable: true);
        var returned = new TaskCompletionSource<BasicReturnEventArgs>(TaskCreationOptions.RunContinuationsAsynchronously);
        ch.BasicReturnAsync += (_, e) => { returned.TrySetResult(e); return Task.CompletedTask; };
        var persistent = new BasicProperties { Persistent = true };
        for (var i = 0; i < 100; i++)
            await ch.BasicPublishAsync("", q, true, persistent, B($"m{i}")); // awaits the broker ack
        var n = await Count(ch, q);
        Check(n == 100, $"count {n}");
        Check(await IsReturned(() => ch.BasicPublishAsync("", $"{P}.nowhere", true, new BasicProperties(), B("x")).AsTask()),
            "unroutable mandatory publish was not returned");
        var ret = await returned.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Check(ret.ReplyCode == 312 && ret.RoutingKey == $"{P}.nowhere", $"basic.return {ret.ReplyCode} {ret.ReplyText}");
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return "100 acked, mandatory return ok";
    }

    static async Task<string?> ConsumeAck()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        var q = $"{P}.consume";
        await Queue(ch, q);
        for (var i = 0; i < 10; i++) await ch.BasicPublishAsync("", q, B($"m{i}"));
        var got = new List<string>();
        var done = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var consumer = new AsyncEventingBasicConsumer(ch);
        consumer.ReceivedAsync += async (_, ea) =>
        {
            got.Add(Encoding.UTF8.GetString(ea.Body.Span));
            await ch.BasicAckAsync(ea.DeliveryTag, false);
            if (got.Count == 10) done.TrySetResult();
        };
        var tag = await ch.BasicConsumeAsync(q, false, consumer);
        await done.Task.WaitAsync(TimeSpan.FromSeconds(5));
        await ch.BasicCancelAsync(tag);
        Check(got.SequenceEqual(Enumerable.Range(0, 10).Select(i => $"m{i}")), $"order {string.Join(",", got)}");
        Check(await Count(ch, q) == 0, "queue not empty after acks");
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> NackRequeue()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync();
        var q = $"{P}.nack";
        await Queue(ch, q);
        await ch.BasicPublishAsync("", q, B("again"));
        var n = 0;
        var done = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var consumer = new AsyncEventingBasicConsumer(ch);
        consumer.ReceivedAsync += async (_, ea) =>
        {
            try
            {
                if (++n == 1)
                {
                    Check(!ea.Redelivered, "first delivery flagged redelivered");
                    await ch.BasicNackAsync(ea.DeliveryTag, false, true);
                }
                else
                {
                    Check(ea.Redelivered && Encoding.UTF8.GetString(ea.Body.Span) == "again", "requeued delivery not redelivered");
                    await ch.BasicAckAsync(ea.DeliveryTag, false);
                    done.TrySetResult();
                }
            }
            catch (Exception e)
            {
                done.TrySetException(e);
            }
        };
        var tag = await ch.BasicConsumeAsync(q, false, consumer);
        await done.Task.WaitAsync(TimeSpan.FromSeconds(5));
        await ch.BasicCancelAsync(tag);
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> RejectDlx()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync();
        string dlx = $"{P}.dlx", dlq = $"{P}.dlq", q = $"{P}.work";
        await ch.ExchangeDeclareAsync(dlx, ExchangeType.Fanout);
        await Queue(ch, dlq);
        await ch.QueueBindAsync(dlq, dlx, "");
        await Queue(ch, q, args: new Dictionary<string, object?> { ["x-dead-letter-exchange"] = dlx });
        await ch.BasicPublishAsync("", q, B("bad"));
        var m = await GetWait(ch, q);
        await ch.BasicRejectAsync(m.DeliveryTag, false);
        var d = await GetWait(ch, dlq);
        var death = (IDictionary<string, object?>)((IList<object?>)d.BasicProperties.Headers!["x-death"]!)[0]!;
        Check(Encoding.UTF8.GetString(d.Body.Span) == "bad", "body");
        Check(Str(death["reason"]) == "rejected" && Str(death["queue"]) == q && Convert.ToInt64(death["count"]) == 1,
            $"x-death {string.Join(", ", death.Select(kv => $"{kv.Key}={Str(kv.Value)}"))}");
        foreach (var n in new[] { q, dlq }) await ch.QueueDeleteAsync(n);
        await ch.ExchangeDeleteAsync(dlx);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> Topic()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        string x = $"{P}.topic", q1 = $"{P}.t1", q2 = $"{P}.t2";
        await ch.ExchangeDeclareAsync(x, ExchangeType.Topic);
        foreach (var (q, k) in new[] { (q1, "a.*"), (q2, "#.z") })
        {
            await Queue(ch, q);
            await ch.QueueBindAsync(q, x, k);
        }
        foreach (var k in new[] { "a.b", "a.b.z", "x.z", "a.z", "b.c" })
            if (await IsReturned(() => ch.BasicPublishAsync(x, k, true, new BasicProperties(), B(k)).AsTask()))
                Check(k == "b.c", $"{k} unroutable");
        uint n1 = await Count(ch, q1), n2 = await Count(ch, q2);
        Check(n1 == 2 && n2 == 3, $"counts {n1},{n2}");
        foreach (var q in new[] { q1, q2 }) await ch.QueueDeleteAsync(q);
        await ch.ExchangeDeleteAsync(x);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> Headers()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        string x = $"{P}.headers", qa = $"{P}.hall", qb = $"{P}.hany";
        await ch.ExchangeDeclareAsync(x, ExchangeType.Headers);
        await Queue(ch, qa);
        await Queue(ch, qb);
        await ch.QueueBindAsync(qa, x, "", new Dictionary<string, object?> { ["x-match"] = "all", ["format"] = "pdf", ["type"] = "report" });
        await ch.QueueBindAsync(qb, x, "", new Dictionary<string, object?> { ["x-match"] = "any", ["format"] = "zip", ["type"] = "log" });
        var cases = new[]
        {
            new Dictionary<string, object?> { ["format"] = "pdf", ["type"] = "report" },
            new Dictionary<string, object?> { ["format"] = "pdf" },
            new Dictionary<string, object?> { ["type"] = "log" },
            new Dictionary<string, object?> { ["format"] = "zip", ["type"] = "x" },
        };
        for (var i = 0; i < cases.Length; i++)
        {
            var h = cases[i];
            if (await IsReturned(() => ch.BasicPublishAsync(x, "", true, new BasicProperties { Headers = h }, B("h")).AsTask()))
                Check(i == 1, $"case {i} unroutable");
        }
        uint na = await Count(ch, qa), nb = await Count(ch, qb);
        Check(na == 1 && nb == 2, $"counts {na},{nb}");
        foreach (var q in new[] { qa, qb }) await ch.QueueDeleteAsync(q);
        await ch.ExchangeDeleteAsync(x);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> Tx()
    {
        await using var c = await Open();
        var ch = await c.CreateChannelAsync();
        var q = $"{P}.tx";
        await Queue(ch, q);
        await ch.TxSelectAsync();
        await ch.BasicPublishAsync("", q, B("rolled back"));
        await ch.TxRollbackAsync();
        Check(await Count(ch, q) == 0, "rollback left a message");
        await ch.BasicPublishAsync("", q, B("committed"));
        await ch.BasicPublishAsync("", q, B("committed"));
        await ch.TxCommitAsync();
        var n = await Count(ch, q);
        Check(n == 2, $"commit count {n}");
        var m = (await ch.BasicGetAsync(q, false))!;
        await ch.BasicAckAsync(m.DeliveryTag, false);
        await ch.TxRollbackAsync();
        await ch.CloseAsync(); // the rolled-back ack leaves the message unacked, so closing requeues it
        ch = await c.CreateChannelAsync();
        Check(await Count(ch, q) == 2, "ack was not rolled back");
        await ch.TxSelectAsync();
        m = (await ch.BasicGetAsync(q, false))!;
        await ch.BasicAckAsync(m.DeliveryTag, false);
        await ch.TxCommitAsync();
        await ch.CloseAsync();
        ch = await c.CreateChannelAsync();
        Check(await Count(ch, q) == 1, "ack was not committed");
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> Prefetch()
    {
        await using var c = await Open();
        var ch = await c.CreateChannelAsync(Confirms);
        var q = $"{P}.prefetch";
        await Queue(ch, q);
        for (var i = 0; i < 20; i++) await ch.BasicPublishAsync("", q, B(i.ToString()));
        await ch.BasicQosAsync(0, 5, false);
        var tags = new ConcurrentQueue<ulong>();
        var consumer = new AsyncEventingBasicConsumer(ch);
        consumer.ReceivedAsync += (_, ea) => { tags.Enqueue(ea.DeliveryTag); return Task.CompletedTask; };
        await ch.BasicConsumeAsync(q, false, consumer);
        await Task.Delay(1000);
        Check(tags.Count == 5, $"received {tags.Count} with prefetch 5");
        await ch.BasicAckAsync(tags.Last(), true);
        await Task.Delay(1000);
        Check(tags.Count == 10, $"received {tags.Count} after acking 5");
        await ch.CloseAsync();
        ch = await c.CreateChannelAsync();
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> Ttl()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        string q1 = $"{P}.ttlq", q2 = $"{P}.ttlm";
        await Queue(ch, q1, args: new Dictionary<string, object?> { ["x-message-ttl"] = 200 });
        await Queue(ch, q2);
        await ch.BasicPublishAsync("", q1, B("x"));
        await ch.BasicPublishAsync("", q2, false, new BasicProperties { Expiration = "200" }, B("short"));
        await ch.BasicPublishAsync("", q2, false, new BasicProperties { Expiration = "60000" }, B("long"));
        await Task.Delay(700);
        Check(await ch.BasicGetAsync(q1, true) is null, "queue TTL did not expire");
        var m = await ch.BasicGetAsync(q2, true);
        var body = m is null ? null : Encoding.UTF8.GetString(m.Body.Span);
        Check(body == "long", $"per-message TTL: got {body}");
        foreach (var q in new[] { q1, q2 }) await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> Priority()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        var q = $"{P}.prio";
        await Queue(ch, q, args: new Dictionary<string, object?> { ["x-max-priority"] = 10 });
        foreach (var p in new byte[] { 1, 9, 5, 0, 7 })
            await ch.BasicPublishAsync("", q, false, new BasicProperties { Priority = p }, B(p.ToString()));
        var got = new List<int>();
        for (var i = 0; i < 5; i++) got.Add(int.Parse(Encoding.UTF8.GetString((await ch.BasicGetAsync(q, true))!.Body.Span)));
        Check(got.SequenceEqual(new[] { 9, 7, 5, 1, 0 }), $"order {string.Join(",", got)}");
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> ExclusiveAutodelete()
    {
        await using var a = await Open();
        await using var b = await Open();
        var cha = await a.CreateChannelAsync();
        var q = (await Queue(cha, "", exclusive: true)).QueueName;
        var chb = await b.CreateChannelAsync();
        await ExpectClose(() => chb.QueueDeclarePassiveAsync(q), 405);
        await a.CloseAsync();
        chb = await b.CreateChannelAsync();
        await ExpectClose(() => chb.QueueDeclarePassiveAsync(q), 404);
        chb = await b.CreateChannelAsync();
        var ad = $"{P}.autodel";
        await Queue(chb, ad, autoDelete: true);
        var tag = await chb.BasicConsumeAsync(ad, false, new AsyncEventingBasicConsumer(chb));
        await chb.BasicCancelAsync(tag);
        await ExpectClose(() => chb.QueueDeclarePassiveAsync(ad), 404);
        await b.CloseAsync();
        return null;
    }

    static async Task<string?> DirectReplyTo()
    {
        await using var c = await Open();
        var client = await c.CreateChannelAsync();
        var server = await c.CreateChannelAsync();
        var q = $"{P}.rpc";
        await Queue(server, q);
        var serve = new AsyncEventingBasicConsumer(server);
        serve.ReceivedAsync += async (_, ea) =>
        {
            var reply = Encoding.UTF8.GetString(ea.Body.Span).ToUpperInvariant();
            await server.BasicPublishAsync("", ea.BasicProperties.ReplyTo!, false,
                new BasicProperties { CorrelationId = ea.BasicProperties.CorrelationId }, B(reply));
            await server.BasicAckAsync(ea.DeliveryTag, false);
        };
        await server.BasicConsumeAsync(q, false, serve);
        var replies = new ConcurrentQueue<string>();
        var rc = new AsyncEventingBasicConsumer(client);
        rc.ReceivedAsync += (_, ea) =>
        {
            replies.Enqueue($"{ea.BasicProperties.CorrelationId}:{Encoding.UTF8.GetString(ea.Body.Span)}");
            return Task.CompletedTask;
        };
        await client.BasicConsumeAsync("amq.rabbitmq.reply-to", true, rc);
        await client.BasicPublishAsync("", q, false,
            new BasicProperties { ReplyTo = "amq.rabbitmq.reply-to", CorrelationId = "c1" }, B("ping"));
        await Until(() => !replies.IsEmpty);
        Check(replies.SequenceEqual(new[] { "c1:PING" }), $"replies [{string.Join(",", replies)}]");
        await server.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> BasicGet()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        var q = $"{P}.get";
        await Queue(ch, q);
        for (var i = 0; i < 3; i++) await ch.BasicPublishAsync("", q, B(i.ToString()));
        var m = (await ch.BasicGetAsync(q, false))!;
        Check(m.MessageCount == 2 && Encoding.UTF8.GetString(m.Body.Span) == "0", $"get-ok message_count {m.MessageCount}");
        await ch.BasicAckAsync(m.DeliveryTag, false);
        await ch.BasicGetAsync(q, true);
        await ch.BasicGetAsync(q, true);
        Check(await ch.BasicGetAsync(q, false) is null, "expected get-empty");
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> CancelNotify()
    {
        await using var c = await Open();
        var ch = await c.CreateChannelAsync();
        var other = await c.CreateChannelAsync();
        var q = $"{P}.cancel";
        await Queue(ch, q);
        var cancelled = new ConcurrentQueue<string>();
        var consumer = new AsyncEventingBasicConsumer(ch);
        // we never call BasicCancel, so an unregistration here is the server-sent basic.cancel
        consumer.UnregisteredAsync += (_, e) =>
        {
            foreach (var t in e.ConsumerTags) cancelled.Enqueue(t);
            return Task.CompletedTask;
        };
        var tag = await ch.BasicConsumeAsync(q, false, consumer);
        await other.QueueDeleteAsync(q);
        await Until(() => !cancelled.IsEmpty);
        Check(cancelled.SequenceEqual(new[] { tag }) && !consumer.IsRunning, $"cancel notifications [{string.Join(",", cancelled)}]");
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> ConnectionBlocked()
    {
        await using var c = await Open();
        var caps = (IDictionary<string, object?>)c.ServerProperties!["capabilities"]!;
        Check(caps.TryGetValue("connection.blocked", out var v) && v is true,
            $"capabilities {string.Join(", ", caps.Select(kv => $"{kv.Key}={kv.Value}"))}");
        c.ConnectionBlockedAsync += (_, _) => Task.CompletedTask;
        c.ConnectionUnblockedAsync += (_, _) => Task.CompletedTask;
        await c.CloseAsync();
        return null;
    }

    static async Task<string?> Heartbeat()
    {
        await using var c = await Open(TimeSpan.FromSeconds(2));
        Check(c.Heartbeat == TimeSpan.FromSeconds(2), $"negotiated heartbeat {c.Heartbeat}");
        await Task.Delay(7000);
        Check(c.IsOpen, $"connection closed while idle: {c.CloseReason}");
        await using var ch = await c.CreateChannelAsync();
        await Queue(ch, "", exclusive: true);
        await c.CloseAsync();
        return "2s heartbeat, idle 7s";
    }

    static async Task<string?> LargeMessage()
    {
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync(Confirms);
        var q = $"{P}.large";
        await Queue(ch, q);
        var body = RandomNumberGenerator.GetBytes(5 * 1024 * 1024);
        await ch.BasicPublishAsync("", q, body);
        var m = await ch.BasicGetAsync(q, true);
        Check(m is not null && SHA256.HashData(m.Body.Span).SequenceEqual(SHA256.HashData(body)), "body mismatch");
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return "5 MiB";
    }

    static async Task<string?> Throughput()
    {
        const int n = 10_000;
        await using var c = await Open();
        await using var ch = await c.CreateChannelAsync();
        var q = $"{P}.tput";
        await Queue(ch, q);
        var body = new byte[100];
        Array.Fill(body, (byte)'x');
        var sw = Stopwatch.StartNew();
        for (var i = 0; i < n; i++) await ch.BasicPublishAsync("", q, body);
        var pub = sw.Elapsed.TotalSeconds;
        Console.Error.WriteLine($"throughput: published {n} in {pub:F2}s");
        await ch.BasicQosAsync(0, 500, false);
        var got = 0;
        var done = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var consumer = new AsyncEventingBasicConsumer(ch);
        consumer.ReceivedAsync += async (_, ea) =>
        {
            got++;
            if (got % 100 == 0 || got == n) await ch.BasicAckAsync(ea.DeliveryTag, true);
            if (got == n) done.TrySetResult();
        };
        sw.Restart();
        var tag = await ch.BasicConsumeAsync(q, false, consumer);
        try
        {
            await done.Task.WaitAsync(TimeSpan.FromSeconds(10));
        }
        catch (TimeoutException)
        {
            throw new Exception($"timed out after {got}");
        }
        var con = sw.Elapsed.TotalSeconds;
        await ch.BasicCancelAsync(tag);
        await ch.QueueDeleteAsync(q);
        await c.CloseAsync();
        return $"publish {n / pub:F0} msg/s, consume {n / con:F0} msg/s";
    }
}
