namespace Roll.Secrets;

/// <summary>
/// X25519 (RFC 7748), which .NET's own cryptography doesn't have: TweetNaCl's
/// <c>crypto_scalarmult</c> (public domain), field elements as sixteen 16-bit limbs. Every step
/// runs the same whatever the scalar's bits (a masked swap, no branches on secrets).
/// </summary>
internal static class X25519
{
    private static readonly long[] A24 = { 0xDB41, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 }; // 121665

    /// <summary>The u-coordinate 9, whose product with a private key is its public key.</summary>
    private static readonly byte[] BasePoint = Base();

    private static byte[] Base()
    {
        var b = new byte[32];
        b[0] = 9;
        return b;
    }

    public static byte[] PublicKey(ReadOnlySpan<byte> scalar) => ScalarMult(scalar, BasePoint);

    /// <summary>RFC 7748's X25519(k, u): the scalar clamped, u's top bit masked.</summary>
    public static byte[] ScalarMult(ReadOnlySpan<byte> scalar, ReadOnlySpan<byte> u)
    {
        if (scalar.Length != 32 || u.Length != 32)
        {
            throw new ArgumentException("X25519 takes a 32-byte scalar and a 32-byte u-coordinate");
        }
        Span<byte> z = stackalloc byte[32];
        scalar.CopyTo(z);
        z[31] = (byte)((z[31] & 127) | 64);
        z[0] &= 248;
        long[] x = new long[16], a = new long[16], b = new long[16], c = new long[16], d = new long[16], e = new long[16], f = new long[16];
        Unpack(x, u);
        Array.Copy(x, b, 16);
        a[0] = d[0] = 1;
        for (int i = 254; i >= 0; --i)
        {
            long r = (z[i >> 3] >> (i & 7)) & 1;
            Swap(a, b, r);
            Swap(c, d, r);
            Add(e, a, c);
            Sub(a, a, c);
            Add(c, b, d);
            Sub(b, b, d);
            Mul(d, e, e);
            Mul(f, a, a);
            Mul(a, c, a);
            Mul(c, b, e);
            Add(e, a, c);
            Sub(a, a, c);
            Mul(b, a, a);
            Sub(c, d, f);
            Mul(a, c, A24);
            Add(a, a, d);
            Mul(c, c, a);
            Mul(a, d, f);
            Mul(d, b, x);
            Mul(b, e, e);
            Swap(a, b, r);
            Swap(c, d, r);
        }
        Invert(c, c);
        Mul(a, a, c);
        var o = new byte[32];
        Pack(o, a);
        z.Clear();
        foreach (var t in new[] { x, a, b, c, d, e, f })
        {
            Array.Clear(t);
        }
        return o;
    }

    private static void Carry(long[] o)
    {
        for (int i = 0; i < 16; i++)
        {
            o[i] += 1L << 16;
            long c = o[i] >> 16;
            o[i < 15 ? i + 1 : 0] += c - 1 + (i == 15 ? 37 * (c - 1) : 0);
            o[i] -= c << 16;
        }
    }

    /// <summary>Swaps p and q when b is 1, leaves them when it's 0, the same work either way.</summary>
    private static void Swap(long[] p, long[] q, long b)
    {
        long mask = ~(b - 1);
        for (int i = 0; i < 16; i++)
        {
            long t = mask & (p[i] ^ q[i]);
            p[i] ^= t;
            q[i] ^= t;
        }
    }

    private static void Pack(byte[] o, long[] n)
    {
        long[] m = new long[16], t = new long[16];
        Array.Copy(n, t, 16);
        Carry(t);
        Carry(t);
        Carry(t);
        for (int j = 0; j < 2; j++)
        {
            m[0] = t[0] - 0xffed;
            for (int i = 1; i < 15; i++)
            {
                m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
                m[i - 1] &= 0xffff;
            }
            m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
            long b = (m[15] >> 16) & 1;
            m[14] &= 0xffff;
            Swap(t, m, 1 - b);
        }
        for (int i = 0; i < 16; i++)
        {
            o[2 * i] = (byte)(t[i] & 0xff);
            o[2 * i + 1] = (byte)((t[i] >> 8) & 0xff);
        }
    }

    private static void Unpack(long[] o, ReadOnlySpan<byte> n)
    {
        for (int i = 0; i < 16; i++)
        {
            o[i] = n[2 * i] + ((long)n[2 * i + 1] << 8);
        }
        o[15] &= 0x7fff;
    }

    private static void Add(long[] o, long[] a, long[] b)
    {
        for (int i = 0; i < 16; i++)
        {
            o[i] = a[i] + b[i];
        }
    }

    private static void Sub(long[] o, long[] a, long[] b)
    {
        for (int i = 0; i < 16; i++)
        {
            o[i] = a[i] - b[i];
        }
    }

    private static void Mul(long[] o, long[] a, long[] b)
    {
        Span<long> t = stackalloc long[31];
        t.Clear();
        for (int i = 0; i < 16; i++)
        {
            for (int j = 0; j < 16; j++)
            {
                t[i + j] += a[i] * b[j];
            }
        }
        for (int i = 0; i < 15; i++)
        {
            t[i] += 38 * t[i + 16];
        }
        for (int i = 0; i < 16; i++)
        {
            o[i] = t[i];
        }
        Carry(o);
        Carry(o);
    }

    /// <summary>i^(p-2), Fermat's inverse.</summary>
    private static void Invert(long[] o, long[] i)
    {
        var c = new long[16];
        Array.Copy(i, c, 16);
        for (int a = 253; a >= 0; a--)
        {
            Mul(c, c, c);
            if (a != 2 && a != 4)
            {
                Mul(c, c, i);
            }
        }
        Array.Copy(c, o, 16);
    }
}
