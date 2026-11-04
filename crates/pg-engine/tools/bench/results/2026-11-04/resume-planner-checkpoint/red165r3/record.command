env GOLDEN_RECORD=127.0.0.1:53310 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a165_cursor_scalar cargo test -p pg-engine --test golden -- --nocapture 
