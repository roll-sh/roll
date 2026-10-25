-- Deeply nested json / jsonb input fails with "stack depth limit exceeded" and its HINT,
-- whichever of the engine's limits (thread stack, nesting depth) it hits first (json 37/38,
-- jsonb 38/39 of the regression suite lost the HINT in builds with smaller frames).
SET max_stack_depth = '100kB';
SELECT repeat('[', 10000)::json;
SELECT repeat('{"a":', 10000)::json;
SELECT repeat('[', 10000)::jsonb;
SELECT repeat('{"a":', 10000)::jsonb;
SELECT repeat('[', 100000)::json;
SELECT repeat('[', 100000)::jsonb;
SELECT json_typeof(repeat('[', 500)::json);
RESET max_stack_depth;
SELECT repeat('[', 100000)::json;
SELECT repeat('{"a":', 100000)::jsonb;
SELECT length((repeat('[', 1000) || repeat(']', 1000))::jsonb::text);
SELECT length((repeat('[', 1000) || repeat(']', 1000))::json::text);
