package sh.roll.secrets;

import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.interfaces.XECPublicKey;
import java.security.spec.NamedParameterSpec;
import java.security.spec.XECPrivateKeySpec;
import java.security.spec.XECPublicKeySpec;
import java.util.Base64;
import javax.crypto.Cipher;
import javax.crypto.KeyAgreement;
import javax.crypto.Mac;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * The X25519 key a sealed resolve is sealed to. Keep it in the process: its public key
 * ({@link #publicKey()}) is all the server needs.
 *
 * <p>A {@code roll-seal-v1} box, as crates/roll-secrets/src/crypto.rs makes it: X25519 between a
 * fresh key pair and the recipient's key, HKDF-SHA256 with salt {@code epk ‖ recipient} and info
 * {@code roll-seal-v1} into an AES-256-GCM key, a random 12-byte nonce, and a context as the
 * associated data, so a box opens only for the purpose it was sealed for. Only the JDK's own
 * providers ({@code XDH}, {@code HmacSHA256}, {@code AES/GCM/NoPadding}).
 */
public final class SealKey {
    private static final byte[] INFO = "roll-seal-v1".getBytes(StandardCharsets.US_ASCII);
    private static final Base64.Encoder B64 = Base64.getUrlEncoder().withoutPadding();
    private static final Base64.Decoder UNB64 = Base64.getUrlDecoder();

    private final PrivateKey key;
    private final byte[] pub;

    private SealKey(PrivateKey key, byte[] pub) {
        this.key = key;
        this.pub = pub;
    }

    /** A fresh key pair. */
    public static SealKey generate() {
        try {
            KeyPair kp = KeyPairGenerator.getInstance("X25519").generateKeyPair();
            return new SealKey(kp.getPrivate(), encode(((XECPublicKey) kp.getPublic()).getU()));
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException("this JDK has no X25519", e);
        }
    }

    /** A key from its 32 raw bytes (RFC 7748's encoding, as the other clients and the server use). */
    public static SealKey fromPrivateBytes(byte[] raw) {
        if (raw.length != 32) {
            throw new IllegalArgumentException("an X25519 private key is 32 bytes");
        }
        try {
            PrivateKey k = KeyFactory.getInstance("XDH").generatePrivate(new XECPrivateKeySpec(NamedParameterSpec.X25519, raw.clone()));
            // The JDK can't give a private key's public one: it's the key agreed with the base point.
            return new SealKey(k, agree(k, BigInteger.valueOf(9)));
        } catch (GeneralSecurityException e) {
            throw new IllegalArgumentException("not a usable X25519 key", e);
        }
    }

    /** The base64url public key, what a sealed resolve's {@code seal_to} takes. */
    public String publicKey() {
        return B64.encodeToString(pub);
    }

    /**
     * Opens a box sealed to this key for {@code context}.
     *
     * @throws IllegalArgumentException when it doesn't open (another key, another context, changed)
     */
    public byte[] open(Sealed box, String context) {
        IllegalArgumentException fail = new IllegalArgumentException("the box doesn't open");
        if (box == null || box.v() != 1 || box.epk() == null || box.nonce() == null || box.ct() == null) {
            throw fail;
        }
        byte[] epk;
        byte[] nonce;
        byte[] ct;
        try {
            epk = UNB64.decode(box.epk());
            nonce = UNB64.decode(box.nonce());
            ct = UNB64.decode(box.ct());
        } catch (IllegalArgumentException e) {
            throw fail;
        }
        if (epk.length != 32 || nonce.length != 12) {
            throw fail;
        }
        try {
            byte[] shared = agree(key, decode(epk));
            byte[] k = boxKey(shared, epk, pub);
            Cipher gcm = Cipher.getInstance("AES/GCM/NoPadding");
            gcm.init(Cipher.DECRYPT_MODE, new SecretKeySpec(k, "AES"), new GCMParameterSpec(128, nonce));
            gcm.updateAAD(context.getBytes(StandardCharsets.UTF_8));
            return gcm.doFinal(ct);
        } catch (GeneralSecurityException | RuntimeException e) {
            throw fail;
        }
    }

    static byte[] agree(PrivateKey k, BigInteger u) throws GeneralSecurityException {
        PublicKey peer = KeyFactory.getInstance("XDH").generatePublic(new XECPublicKeySpec(NamedParameterSpec.X25519, u));
        KeyAgreement ka = KeyAgreement.getInstance("XDH");
        ka.init(k);
        ka.doPhase(peer, true);
        // The JDK refuses an all-zero result (a small-order point) itself.
        return ka.generateSecret();
    }

    /** HKDF-SHA256 (RFC 5869) of the shared secret, salt epk ‖ recipient, info roll-seal-v1, 32 bytes. */
    static byte[] boxKey(byte[] shared, byte[] epk, byte[] recipient) throws GeneralSecurityException {
        boolean zero = true;
        for (byte b : shared) {
            zero &= b == 0;
        }
        if (zero) {
            throw new GeneralSecurityException("not a usable X25519 key");
        }
        byte[] salt = new byte[64];
        System.arraycopy(epk, 0, salt, 0, 32);
        System.arraycopy(recipient, 0, salt, 32, 32);
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec(salt, "HmacSHA256"));
        byte[] prk = mac.doFinal(shared);
        mac.init(new SecretKeySpec(prk, "HmacSHA256"));
        mac.update(INFO);
        mac.update((byte) 1);
        return mac.doFinal(); // one block: exactly the 32 bytes needed
    }

    /** A u-coordinate as RFC 7748's 32 little-endian bytes. */
    static byte[] encode(BigInteger u) {
        byte[] be = u.toByteArray();
        byte[] out = new byte[32];
        for (int i = 0; i < 32 && i < be.length; i++) {
            out[i] = be[be.length - 1 - i];
        }
        return out;
    }

    /** RFC 7748's decoding: little-endian, the top bit masked. */
    static BigInteger decode(byte[] le) {
        byte[] be = new byte[32];
        for (int i = 0; i < 32; i++) {
            be[i] = le[31 - i];
        }
        be[0] &= 0x7f;
        return new BigInteger(1, be);
    }

    @Override
    public String toString() {
        return "SealKey[" + publicKey() + "]";
    }
}
