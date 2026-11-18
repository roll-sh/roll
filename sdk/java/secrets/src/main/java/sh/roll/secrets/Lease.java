package sh.roll.secrets;

import java.util.Map;

/**
 * A dynamic credential's lease. Times are Unix milliseconds; {@code revokedAt} is null while it
 * lives.
 */
public record Lease(String id, String environment, String path, String key, String kind, String consumer,
                    long issuedAt, long expiresAt, Long revokedAt) {
    static Lease of(Map<String, Object> o) {
        Object revoked = o.get("revoked_at");
        return new Lease(Json.str(o, "id"), Json.str(o, "environment"), Json.str(o, "path"), Json.str(o, "key"),
                Json.str(o, "kind"), Json.str(o, "consumer"), Json.num(o, "issued_at"), Json.num(o, "expires_at"),
                revoked instanceof Number ? ((Number) revoked).longValue() : null);
    }
}
